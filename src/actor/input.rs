//! Keyboard, mouse and native gesture arbitration.
//!
//! The HID tap callback runs on a dedicated thread ([`TapThread`]) and only
//! consults in-memory state: it decides pass/consume, sends to other actors,
//! and forwards work that needs WindowServer to this actor ([`TapEvent`]).
//! WindowServer holds every masked key and click behind that callback, so no
//! WindowServer, AX or other blocking call may run on its thread.

use std::cell::{Cell, RefCell};
use std::panic::AssertUnwindSafe;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use objc2_core_foundation::{CGPoint, CGRect};
use objc2_core_graphics::{
    CGDisplayBounds, CGEvent, CGEventField, CGEventFlags, CGEventMask, CGEventSource,
    CGEventSourceStateID, CGEventTapLocation as CGTapLoc, CGEventTapOptions as CGTapOpt,
    CGEventTapPlacement as CGTapPlace, CGEventTapProxy, CGEventType,
};
use parking_lot::Mutex;
use tracing::{debug, error, trace, warn};

use super::reactor::{self, Event};
use super::stack_line;
use crate::actor;
use crate::actor::spaces::ForwardedSpaceState;
use crate::actor::wm_controller::{self, WmCommand, WmEvent};
use crate::common::collections::{HashMap, HashSet};
use crate::common::config::{
    BindingModeSpecs, Config, DragDropSettings, EventTapPlacement, HorizontalMouseWarp, LayoutMode,
    MouseAction, MouseModifier, StackLineHoverMode,
};
use crate::sys::event::{self, Hotkey, KeyCode};
use crate::sys::event_tap::TapThread;
use crate::sys::hotkey::{
    Modifiers, is_modifier_key, key_code_from_event, modifier_key_is_active,
    modifiers_from_flags_with_keys,
};
use crate::sys::screen::{CoordinateConverter, SpaceId};
use crate::sys::{gesture, power, window_server};
use crate::ui::stack_line::point_hits_indicator_frame;

const MOUSE_MOVE_MIN_INTERVAL_NS_NORMAL: u64 = 16_000_000; // 16ms ~= 62 Hz
const MOUSE_MOVE_MIN_INTERVAL_NS_LOW_POWER: u64 = 32_000_000; // 32ms ~= 31 Hz

/// Longest the tap callback waits for the input state lock. The actor only
/// holds it for in-memory work, so this is never reached unless something is
/// wrong; then the event passes through untouched rather than stalling input.
const CALLBACK_LOCK_WAIT: Duration = Duration::from_millis(1);
const FORWARD_CAPACITY: usize = 64;

/// Events the callback passed through because the state lock was busy.
static CALLBACK_LOCK_MISSES: AtomicU64 = AtomicU64::new(0);

/// The installed HID tap. Unit tests get a recorder with the same interface,
/// so no test inserts a real tap into the live session's HID event chain.
#[cfg(not(test))]
type Tap = crate::sys::event_tap::EventTap;
#[cfg(test)]
type Tap = tests::RecordedTap;

#[derive(Debug)]
pub enum Request {
    Warp(CGPoint),
    HideOnFocus,
    EnforceHidden,
    SpaceStateUpdated(ForwardedSpaceState, CoordinateConverter),
    SetEventProcessing(bool),
    SetFocusFollowsMouseEnabled(bool),
    EnableHotkeys,
    SetBindingMode(String),
    KeyboardLayoutChanged,
    ConfigUpdated(Config),
    LayoutModesChanged(Vec<(SpaceId, crate::common::config::LayoutMode)>),
    SetLowPowerMode(bool),
    SetMissionControlActive(bool),
    ReleaseMissionControl,
}

/// Work the tap callback hands to the actor because it needs WindowServer.
/// Sent with `try_send` on a bounded channel; a full channel drops the work
/// (counted) and the event itself still passes through.
#[derive(Debug, PartialEq)]
enum TapEvent {
    ShowMouse,
    Warp(CGPoint),
    StackLineMove { point: CGPoint, rect_hit: bool },
}

pub struct Input {
    events_tx: reactor::Sender,
    requests_rx: Option<Receiver>,
    forward_rx: Option<tokio::sync::mpsc::Receiver<TapEvent>>,
    recovery_rx: Option<tokio::sync::mpsc::UnboundedReceiver<Recovery>>,
    state: Arc<Mutex<State>>,
    // One context for every tap generation; freed on the tap thread after
    // the last tap, so a callback in flight there never sees it go away.
    callback_ctx: std::mem::ManuallyDrop<Box<CallbackCtx>>,
    event_mask: Cell<CGEventMask>,
    hide_count: Cell<u32>,
    mouse_hides_on_focus: Cell<bool>,
    gesture_control: super::gesture::Control,
    tap: RefCell<Option<Tap>>,
    tap_thread: RefCell<Option<Arc<TapThread>>>,
    tap_generation: Cell<u64>,
    /// `settings.event_tap_placement`, for the next tap created.
    tap_placement: Cell<EventTapPlacement>,
    binding_mode_specs: RefCell<BindingModeSpecs>,
    hotkeys_active: Cell<bool>,
}

impl Drop for Input {
    fn drop(&mut self) {
        // Unregister callbacks before their state is destroyed.
        self.gesture_control.stop(&self.events_tx);
        self.tap.get_mut().take();
        // SAFETY: taken exactly once, here.
        let ctx = unsafe { std::mem::ManuallyDrop::take(&mut self.callback_ctx) };
        match self.tap_thread.get_mut().take() {
            Some(thread) => thread.retire(Box::into_raw(ctx).cast(), drop_callback_ctx),
            None => drop(ctx),
        }
    }
}

unsafe fn drop_callback_ctx(ptr: *mut std::ffi::c_void) {
    unsafe { drop(Box::from_raw(ptr as *mut CallbackCtx)) };
}

/// Shared by the actor and the tap callback on the event-tap thread.
///
/// Never hold the lock across a WindowServer, AX or other blocking call: the
/// callback waits at most [`CALLBACK_LOCK_WAIT`] for it and then passes the
/// event through, so a slow holder costs hotkeys, never input.
struct State {
    focus_follows_mouse_config_enabled: bool,
    default_layout_mode: LayoutMode,
    converter: CoordinateConverter,
    screens: Vec<CGRect>,
    event_processing_enabled: bool,
    focus_follows_mouse_enabled: bool,
    stack_line_enabled: bool,
    stack_line_hover_mode: StackLineHoverMode,
    disable_hotkey_active: bool,
    low_power_mode: bool,
    pressed_keys: HashSet<KeyCode>,
    current_flags: CGEventFlags,
    screen_spaces: Vec<(CGRect, SpaceId)>,
    layout_mode_by_space: HashMap<SpaceId, crate::common::config::LayoutMode>,
    last_stack_line_hit: Option<bool>,
    mouse_features_enabled: bool,
    mouse_settings: DragDropSettings,
    captured_button: Option<crate::actor::drag::MouseButton>,
    gesture_settings: super::gesture::Settings,
    mission_control_active: bool,
    mission_control_tx: Option<super::mission_control::Sender>,
    mouse_move_last_timestamp: Option<u64>,
    mouse_move_min_interval_ticks: u64,
    mouse_location: CGPoint,
    horizontal_mouse_warp: Option<HorizontalMouseWarp>,
    warp_screens: Vec<CGRect>,
    gesture_filter: gesture::Filter,
    gesture_control: super::gesture::Control,
    disable_hotkey: Option<Hotkey>,
    hotkeys: Vec<HashMap<Hotkey, Vec<WmCommand>>>,
    mode_names: Vec<String>,
    mode_indices: HashMap<String, usize>,
    active_mode: usize,
    /// Mirrors the actor's hide count so the callback knows when a move must
    /// show the cursor (a WindowServer call, forwarded).
    cursor_hidden: bool,
    /// Occlusion of the last sampled move that hit an indicator rectangle,
    /// answered by the actor; clicks consult it instead of WindowServer.
    stack_line_occluded: bool,
    events_tx: reactor::Sender,
    wm_sender: wm_controller::Sender,
    stack_line_tx: stack_line::Sender,
    mouse_focus_publisher: reactor::MouseFocusPublisher,
    drag_motion_publisher: crate::actor::drag::DragMotionPublisher,
    native_motion_active: Arc<AtomicBool>,
    stack_line_hit_rects: stack_line::SharedHitRects,
    forward_tx: tokio::sync::mpsc::Sender<TapEvent>,
    forward_drops: u64,
}

pub type Sender = actor::Sender<Request>;
pub type Receiver = actor::Receiver<Request>;

struct CallbackCtx {
    state: Arc<Mutex<State>>,
    recovery_tx: tokio::sync::mpsc::UnboundedSender<Recovery>,
    tap_generation: AtomicU64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Recovery {
    TapInvalidated(u64),
    /// WindowServer disabled the tap and the trampoline re-enabled it; verify
    /// and reconcile outside the callback.
    TapDisabled(u64),
    NativeGestureHeld,
}

impl Input {
    fn desired_event_mask(&self, state: &State) -> CGEventMask {
        let disable_hotkey = &state.disable_hotkey;
        let keyed_disable =
            disable_hotkey.as_ref().is_some_and(|key| !is_modifier_key(key.key_code));
        let hotkeys_enabled = state.hotkeys.iter().any(|map| !map.is_empty());
        let mut mask = build_event_mask(
            hotkeys_enabled || keyed_disable || state.mission_control_active,
            hotkeys_enabled || disable_hotkey.is_some(),
            (state.event_processing_enabled
                && (state.stack_line_enabled
                    || self.mouse_hides_on_focus.get()
                    || state.horizontal_mouse_warp.is_some()
                    || (state.focus_follows_mouse_config_enabled
                        && state.focus_follows_mouse_enabled)))
                || state.mission_control_active,
            state.event_processing_enabled
                && (state.stack_line_enabled || self.mouse_hides_on_focus.get()),
            // Mouse-up delivery is part of the stable configured mask. Drag
            // start/stop is frequent enough that rebuilding the WindowServer
            // tap costs more than filtering these releases in the callback.
            state.event_processing_enabled,
            keyed_disable,
        );
        if state.mission_control_active {
            mask |= (1u64 << CGEventType::LeftMouseDown.0)
                | (1u64 << CGEventType::LeftMouseUp.0)
                | (1u64 << CGEventType::LeftMouseDragged.0)
                | (1u64 << CGEventType::RightMouseDown.0)
                | (1u64 << CGEventType::RightMouseDragged.0)
                | (1u64 << CGEventType::RightMouseUp.0)
                | (1u64 << CGEventType::ScrollWheel.0);
        }
        if state.event_processing_enabled && state.mouse_features_enabled {
            if state.mouse_settings.action1 != MouseAction::None {
                mask |= (1u64 << CGEventType::LeftMouseDown.0)
                    | (1u64 << CGEventType::LeftMouseDragged.0);
            }
            if state.mouse_settings.action2 != MouseAction::None {
                mask |= (1u64 << CGEventType::RightMouseDown.0)
                    | (1u64 << CGEventType::RightMouseDragged.0);
            }
        }
        if state.event_processing_enabled && state.horizontal_mouse_warp.is_some() {
            mask |= (1u64 << CGEventType::LeftMouseDragged.0)
                | (1u64 << CGEventType::RightMouseDragged.0);
        }
        if state.gesture_settings.enabled() {
            mask |= gesture::EVENT_MASK;
        }
        mask
    }

    fn create_tap_with_mask(&self, mask: CGEventMask) -> Option<Tap> {
        let tap_generation = self.tap_generation.get().wrapping_add(1);
        self.callback_ctx.tap_generation.store(tap_generation, Ordering::Release);
        let ctx_ptr = &**self.callback_ctx as *const CallbackCtx as *mut std::ffi::c_void;

        let mut thread = self.tap_thread.borrow_mut();
        if thread.is_none() {
            *thread = TapThread::spawn();
            if thread.is_none() {
                warn!(
                    "Could not start the event tap thread; servicing the tap on the input thread"
                );
            }
        }
        let placement = CGTapPlace::from(self.tap_placement.get());
        let tap = unsafe {
            Tap::new(
                CGTapLoc::HIDEventTap,
                CGTapOpt::Default,
                mask,
                placement,
                Some(input_callback),
                ctx_ptr,
                None,
                Some(event_tap_reenabled),
                Some(event_tap_invalidated),
                thread.as_ref(),
            )
        };

        if tap.is_some() {
            self.tap_generation.set(tap_generation);
            debug!(tap_generation, mask, ?placement, "Created the HID event tap");
        }
        tap
    }

    /// The only way rift creates its tap: the first one, mask changes,
    /// placement changes and recovery all come through here.
    fn rebuild_event_tap_mask_if_needed(&self) {
        let next_mask = self.desired_event_mask(&self.state.lock());
        let placement = CGTapPlace::from(self.tap_placement.get());
        let installed = self.tap.borrow().as_ref().map(|tap| tap.placement());
        if next_mask == self.event_mask.get() && (next_mask == 0 || installed == Some(placement)) {
            return;
        }

        self.reset_gestures();
        self.tap.borrow_mut().take();
        if next_mask == 0 {
            self.event_mask.set(0);
            return;
        }
        let Some(new_tap) = self.create_tap_with_mask(next_mask) else {
            warn!("Failed to rebuild event tap with updated mask");
            return;
        };

        *self.tap.borrow_mut() = Some(new_tap);
        self.event_mask.set(next_mask);
    }

    fn rebuild_invalidated_event_tap(&self, generation: u64) {
        if generation != self.tap_generation.get() {
            debug!(generation, "Ignoring invalidation from a replaced event tap");
            return;
        }

        self.tap.borrow_mut().take();
        self.reconcile_after_tap_reenabled();
        self.rebuild_event_tap_mask_if_needed();
    }

    /// Runs on the actor loop, never inside the tap callback: the enabled
    /// query and the flags read are synchronous WindowServer calls.
    fn on_tap_disabled(&self, generation: u64) {
        if generation != self.tap_generation.get() {
            debug!(generation, "Ignoring disable notice from a replaced event tap");
            return;
        }
        let enabled = self.tap.borrow().as_ref().is_some_and(|tap| tap.is_enabled());
        if enabled {
            self.reconcile_after_tap_reenabled();
        } else {
            error!("Event tap did not re-enable; scheduling tap recreation");
            self.rebuild_invalidated_event_tap(generation);
        }
    }

    pub fn new(
        config: Config,
        events_tx: reactor::Sender,
        requests_rx: Receiver,
        wm_sender: wm_controller::Sender,
        stack_line_tx: stack_line::Sender,
        mission_control_tx: Option<super::mission_control::Sender>,
        stack_line_hit_rects: stack_line::SharedHitRects,
        native_motion_active: Arc<AtomicBool>,
    ) -> Self {
        let disable_hotkey = config
            .settings
            .focus_follows_mouse_disable_hotkey
            .clone()
            .and_then(|spec| spec.to_hotkey());
        let gesture_control = super::gesture::Control::new(&config);
        crate::sys::event_tap::set_timeout_limit(config.settings.event_tap_timeout_limit);
        let low_power_mode = power::is_low_power_mode_enabled();
        let (forward_tx, forward_rx) = tokio::sync::mpsc::channel(FORWARD_CAPACITY);
        let (recovery_tx, recovery_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut state = State {
            focus_follows_mouse_config_enabled: config.settings.focus_follows_mouse,
            default_layout_mode: config.settings.layout.mode,
            converter: CoordinateConverter::default(),
            screens: Vec::new(),
            event_processing_enabled: false,
            focus_follows_mouse_enabled: true,
            stack_line_enabled: config.settings.ui.stack_line.enabled,
            stack_line_hover_mode: config.settings.ui.stack_line.hover,
            disable_hotkey_active: false,
            low_power_mode,
            pressed_keys: HashSet::with_capacity_and_hasher(256, Default::default()),
            current_flags: CGEventFlags::empty(),
            screen_spaces: Vec::new(),
            layout_mode_by_space: HashMap::default(),
            last_stack_line_hit: None,
            mouse_features_enabled: config.settings.drag_drop.enabled,
            mouse_settings: config.settings.drag_drop,
            captured_button: None,
            gesture_settings: super::gesture::Settings::new(&config),
            mission_control_active: false,
            mission_control_tx,
            mouse_move_last_timestamp: None,
            mouse_move_min_interval_ticks: mouse_move_sampling_profile(low_power_mode),
            mouse_location: CGPoint::new(0.0, 0.0),
            horizontal_mouse_warp: config.settings.horizontal_mouse_warp,
            warp_screens: Vec::new(),
            gesture_filter: gesture::Filter::default(),
            gesture_control: gesture_control.clone(),
            disable_hotkey,
            hotkeys: Vec::new(),
            mode_names: Vec::new(),
            mode_indices: HashMap::default(),
            active_mode: 0,
            cursor_hidden: false,
            stack_line_occluded: false,
            events_tx: events_tx.clone(),
            wm_sender,
            stack_line_tx,
            mouse_focus_publisher: reactor::MouseFocusPublisher::default(),
            drag_motion_publisher: crate::actor::drag::DragMotionPublisher::default(),
            native_motion_active,
            stack_line_hit_rects,
            forward_tx,
            forward_drops: 0,
        };
        state.disable_hotkey_active = state
            .disable_hotkey
            .as_ref()
            .map(|target| state.compute_disable_hotkey_active(target))
            .unwrap_or(false);
        state.install_binding_specs(&config.binding_mode_specs, false);
        let state = Arc::new(Mutex::new(state));
        Input {
            events_tx,
            requests_rx: Some(requests_rx),
            forward_rx: Some(forward_rx),
            recovery_rx: Some(recovery_rx),
            callback_ctx: std::mem::ManuallyDrop::new(Box::new(CallbackCtx {
                state: state.clone(),
                recovery_tx,
                tap_generation: AtomicU64::new(0),
            })),
            state,
            event_mask: Cell::new(0),
            hide_count: Cell::new(0),
            mouse_hides_on_focus: Cell::new(config.settings.mouse_hides_on_focus),
            gesture_control,
            tap: RefCell::new(None),
            tap_thread: RefCell::new(None),
            tap_generation: Cell::new(0),
            tap_placement: Cell::new(config.settings.event_tap_placement),
            binding_mode_specs: RefCell::new(config.binding_mode_specs),
            hotkeys_active: Cell::new(false),
        }
    }

    pub async fn run(mut self) {
        let mut requests_rx = self.requests_rx.take().unwrap();
        let mut forward_rx = self.forward_rx.take().unwrap();
        let mut recovery_rx = self.recovery_rx.take().unwrap();

        let this = self;

        this.rebuild_event_tap_mask_if_needed();
        let _gesture_monitor = this.gesture_control.start(this.events_tx.clone());

        if this.mouse_hides_on_focus.get() {
            if let Err(e) = window_server::allow_hide_mouse() {
                error!(
                    "Could not enable mouse hiding: {e:?}. \
                    mouse_hides_on_focus will have no effect."
                );
            }
        }

        loop {
            let hold_deadline = this.state.lock().gesture_filter.hold_deadline();
            tokio::select! {
                _ = async { crate::sys::timer::Timer::sleep(hold_deadline.unwrap().saturating_duration_since(std::time::Instant::now())).await }, if hold_deadline.is_some() => {
                    // Copy the ownership first: the callback locks the state and then
                    // the ownership, so never take them in the other order.
                    let owner = *this.gesture_control.ownership_guard();
                    this.state.lock().gesture_filter.release_expired(owner);
                }

                // select evaluates disabled futures too; defer timer creation
                // so healthy taps allocate no timer and schedule no wakeup.
                _ = async { crate::sys::timer::Timer::sleep(Duration::from_secs(1)).await },
                    if this.tap.borrow().is_none() && this.desired_event_mask(&this.state.lock()) != 0 => {
                    this.rebuild_event_tap_mask_if_needed();
                    if this.tap.borrow().is_some() { this.reconcile_after_tap_reenabled(); }
                }
                maybe_recovery = recovery_rx.recv() => {
                    let Some(recovery) = maybe_recovery else { break };
                    match recovery {
                        Recovery::NativeGestureHeld => {}
                        Recovery::TapInvalidated(generation) => {
                            this.rebuild_invalidated_event_tap(generation);
                        }
                        Recovery::TapDisabled(generation) => {
                            this.on_tap_disabled(generation);
                        }
                    }
                }
                maybe_forward = forward_rx.recv() => {
                    let Some(event) = maybe_forward else { break };
                    this.on_tap_event(event);
                }
                maybe_request = requests_rx.recv() => {
                    let Some((span, request)) = maybe_request else { break };
                    let _guard = span.enter();
                    this.on_request(request);
                }
            }
        }
    }

    fn on_request(&self, request: Request) {
        // Cursor requests are WindowServer calls; keep them off the state lock.
        match &request {
            Request::Warp(point) => {
                if let Err(e) = event::warp_mouse(*point) {
                    warn!("Failed to warp mouse: {e:?}");
                }
                if self.mouse_hides_on_focus.get() && self.hide_count.get() == 0 {
                    debug!("Hiding mouse");
                    self.hide_mouse();
                }
                return;
            }
            Request::HideOnFocus => {
                if self.mouse_hides_on_focus.get() && self.hide_count.get() == 0 {
                    debug!("Hiding mouse after window focus changed");
                    self.hide_mouse();
                }
                return;
            }
            Request::EnforceHidden => {
                if self.hide_count.get() > 0 {
                    self.hide_mouse();
                }
                return;
            }
            _ => {}
        }
        let reset_gestures = match &request {
            Request::SpaceStateUpdated(snapshot, _) => !snapshot
                .screens
                .iter()
                .filter_map(|s| s.space.map(|space| (s.frame, space)))
                .eq(self.state.lock().screen_spaces.iter().copied()),
            Request::LayoutModesChanged(modes) => {
                let state = self.state.lock();
                modes.len() != state.layout_mode_by_space.len()
                    || modes
                        .iter()
                        .any(|(space, mode)| state.layout_mode_by_space.get(space) != Some(mode))
            }
            Request::SetEventProcessing(enabled) => {
                *enabled != self.state.lock().event_processing_enabled
            }
            Request::SetMissionControlActive(active) => {
                *active != self.state.lock().mission_control_active
            }
            Request::ConfigUpdated(_) | Request::ReleaseMissionControl => true,
            _ => false,
        };
        if reset_gestures {
            self.reset_gestures();
        }
        let configure_gestures =
            reset_gestures || matches!(&request, Request::SpaceStateUpdated(..));
        // ScreenInfo.frame excludes menu bar/Dock areas; cursor edges use raw CG
        // bounds. Query them before taking the lock.
        let mut warp_bounds = match &request {
            Request::SpaceStateUpdated(space_state, _) => Some(
                space_state
                    .screens
                    .iter()
                    .map(|screen| CGDisplayBounds(screen.id.as_u32()))
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        };
        let mut should_rebuild_mask = false;
        let mut show_mouse = false;
        let mut guard = self.state.lock();
        let state = &mut *guard;
        match request {
            Request::ReleaseMissionControl => {
                state.mission_control_tx.take();
                state.mission_control_active = false;
                should_rebuild_mask = true;
            }
            Request::SetMissionControlActive(active) => {
                state.mission_control_active = active;
                state.reset_mouse_move_sample_gate();
                show_mouse = active && self.hide_count.get() > 0;
                should_rebuild_mask = true;
            }
            Request::Warp(_) | Request::HideOnFocus | Request::EnforceHidden => {}
            Request::SpaceStateUpdated(space_state, converter) => {
                state.screens = space_state.screens.iter().map(|screen| screen.frame).collect();
                state.warp_screens = warp_bounds.take().unwrap_or_default();
                let direction = state.horizontal_mouse_warp;
                sort_warp_screens(&mut state.warp_screens, direction);
                state.screen_spaces = space_state
                    .screens
                    .into_iter()
                    .filter_map(|screen| screen.space.map(|space| (screen.frame, space)))
                    .collect();
                state.converter = converter;
            }
            Request::SetEventProcessing(enabled) => {
                if state.captured_button.take().is_some() {
                    self.events_tx.send(Event::DragCancel);
                }
                state.event_processing_enabled = enabled;
                state.reset(enabled);
                if enabled {
                    state.reset_mouse_move_sample_gate();
                }
                should_rebuild_mask = true;
            }
            Request::SetFocusFollowsMouseEnabled(enabled) => {
                debug!(
                    "focus_follows_mouse temporarily {}",
                    if enabled { "enabled" } else { "disabled" }
                );
                state.focus_follows_mouse_enabled = enabled;
                state.reset(enabled);
                if enabled {
                    state.reset_mouse_move_sample_gate();
                }
                should_rebuild_mask = true;
            }
            Request::EnableHotkeys => {
                if !self.hotkeys_active.replace(true) {
                    state.rebuild_binding_maps(&self.binding_mode_specs.borrow());
                    should_rebuild_mask = true;
                }
            }
            Request::SetBindingMode(target) => state.transition_binding_mode(&target),
            Request::KeyboardLayoutChanged => {
                if self.hotkeys_active.get() {
                    state.rebuild_binding_maps(&self.binding_mode_specs.borrow());
                    should_rebuild_mask = true;
                }
            }
            Request::ConfigUpdated(new_config) => {
                if *self.binding_mode_specs.borrow() != new_config.binding_mode_specs {
                    self.install_binding_specs(state, new_config.binding_mode_specs.clone());
                }
                crate::sys::event_tap::set_timeout_limit(
                    new_config.settings.event_tap_timeout_limit,
                );
                // The rebuild below re-creates the tap when the placement changed.
                self.tap_placement.set(new_config.settings.event_tap_placement);
                let cancel_captured_drag = state.captured_button.is_some()
                    && (!new_config.settings.drag_drop.enabled
                        || new_config.settings.drag_drop != state.mouse_settings);
                if cancel_captured_drag {
                    state.captured_button = None;
                    self.events_tx.send(Event::DragCancel);
                }
                state.gesture_settings = super::gesture::Settings::new(&new_config);
                let mouse_hides_on_focus = new_config.settings.mouse_hides_on_focus;
                let focus_follows_mouse_config_enabled = new_config.settings.focus_follows_mouse;
                let stack_line_enabled = new_config.settings.ui.stack_line.enabled;
                let stack_line_hover_mode = new_config.settings.ui.stack_line.hover;
                let default_layout_mode = new_config.settings.layout.mode;
                let mouse_features_enabled = new_config.settings.drag_drop.enabled;
                let disable_hotkey = new_config
                    .settings
                    .focus_follows_mouse_disable_hotkey
                    .clone()
                    .and_then(|spec| spec.to_hotkey());
                state.disable_hotkey = disable_hotkey;
                {
                    let prev_mouse_hides_on_focus = self.mouse_hides_on_focus.get();
                    let prev_focus_follows_mouse_config_enabled =
                        state.focus_follows_mouse_config_enabled;
                    let prev_stack_line_enabled = state.stack_line_enabled;
                    let prev_stack_line_hover_mode = state.stack_line_hover_mode;
                    self.mouse_hides_on_focus.set(mouse_hides_on_focus);
                    state.focus_follows_mouse_config_enabled = focus_follows_mouse_config_enabled;
                    let direction = new_config.settings.horizontal_mouse_warp;
                    if state.horizontal_mouse_warp != direction {
                        state.horizontal_mouse_warp = direction;
                        sort_warp_screens(&mut state.warp_screens, direction);
                    }
                    state.stack_line_enabled = stack_line_enabled;
                    state.stack_line_hover_mode = stack_line_hover_mode;
                    state.default_layout_mode = default_layout_mode;
                    state.mouse_features_enabled = mouse_features_enabled;
                    state.mouse_settings = new_config.settings.drag_drop;
                    let prev_active = state.disable_hotkey_active;
                    state.disable_hotkey_active = state
                        .disable_hotkey
                        .as_ref()
                        .map(|target| state.compute_disable_hotkey_active(target))
                        .unwrap_or(false);
                    if prev_active && !state.disable_hotkey_active {
                        state.reset(true);
                        state.reset_mouse_move_sample_gate();
                    }
                    if prev_focus_follows_mouse_config_enabled
                        != state.focus_follows_mouse_config_enabled
                        || prev_stack_line_enabled != state.stack_line_enabled
                        || prev_stack_line_hover_mode != state.stack_line_hover_mode
                    {
                        state.reset_mouse_sampling();
                        state.reset_mouse_move_sample_gate();
                    }
                    if prev_mouse_hides_on_focus
                        && !mouse_hides_on_focus
                        && self.hide_count.get() > 0
                    {
                        debug!("Showing mouse after disabling mouse_hides_on_focus");
                        show_mouse = true;
                    }
                }
                should_rebuild_mask = true;
            }
            Request::LayoutModesChanged(modes) => {
                state.layout_mode_by_space.clear();
                for (space, mode) in modes {
                    state.layout_mode_by_space.insert(space, mode);
                }
                debug!(
                    "Updated layout modes for {} spaces",
                    state.layout_mode_by_space.len()
                );
            }
            Request::SetLowPowerMode(enabled) => {
                if state.low_power_mode != enabled {
                    debug!("low_power_mode changed in event tap: {}", enabled);
                    state.low_power_mode = enabled;
                    state.reset_mouse_sampling();
                    state.mouse_move_min_interval_ticks = mouse_move_sampling_profile(enabled);
                    state.reset_mouse_move_sample_gate();
                }
            }
        }
        if configure_gestures {
            self.gesture_control.configure(
                state.gesture_settings,
                state.event_processing_enabled && !state.mission_control_active,
                state
                    .screen_spaces
                    .iter()
                    .map(|&(frame, space)| {
                        (
                            frame,
                            space,
                            state
                                .layout_mode_by_space
                                .get(&space)
                                .copied()
                                .unwrap_or(state.default_layout_mode),
                        )
                    })
                    .collect(),
            );
        }
        drop(guard);

        if show_mouse {
            self.show_mouse();
        }
        if should_rebuild_mask {
            self.rebuild_event_tap_mask_if_needed();
        }
    }

    /// Work the tap callback could not do itself: each arm is a WindowServer call.
    fn on_tap_event(&self, event: TapEvent) {
        match event {
            TapEvent::ShowMouse => self.show_mouse(),
            TapEvent::Warp(target) => {
                if let Err(error) = event::warp_mouse(target) {
                    warn!(?error, "Horizontal mouse warp failed");
                }
            }
            TapEvent::StackLineMove { point, rect_hit } => {
                let hits = rect_hit && !window_server::is_point_occluded_by_external_window(point);
                let mut state = self.state.lock();
                if rect_hit {
                    state.stack_line_occluded = !hits;
                }
                // Click mode only needs hit-test transitions for cursor feedback.
                // Hover mode forwards samples so the actor can detect segment changes.
                if (state.stack_line_hover_mode != StackLineHoverMode::Click && hits)
                    || state.last_stack_line_hit != Some(hits)
                {
                    state.last_stack_line_hit = Some(hits);
                    let _ = state
                        .stack_line_tx
                        .try_send(stack_line::Event::MouseMoved { point, hits_indicator: hits });
                }
            }
        }
    }

    fn hide_mouse(&self) {
        if let Err(e) = event::hide_mouse() {
            warn!("Failed to hide mouse: {e:?}");
        }
        self.hide_count.set(self.hide_count.get() + 1);
        self.state.lock().cursor_hidden = true;
    }

    fn show_mouse(&self) {
        while self.hide_count.get() > 0 {
            if let Err(e) = event::show_mouse() {
                warn!("Failed to show mouse: {e:?}");
            }
            self.hide_count.set(self.hide_count.get() - 1);
        }
        self.state.lock().cursor_hidden = false;
    }

    fn install_binding_specs(&self, state: &mut State, specs: BindingModeSpecs) {
        state.install_binding_specs(&specs, self.hotkeys_active.get());
        *self.binding_mode_specs.borrow_mut() = specs;
    }

    fn reset_gestures(&self) {
        self.gesture_control.reset(&self.events_tx);
        self.state.lock().gesture_filter.reset();
    }

    fn reconcile_after_tap_reenabled(&self) {
        self.gesture_control.reset(&self.events_tx);
        let flags = CGEventSource::flags_state(CGEventSourceStateID::HIDSystemState);
        let mut state = self.state.lock();
        state.gesture_filter.reset();
        if state.captured_button.take().is_some() {
            state.events_tx.send(Event::DragCancel);
        }
        debug!(?flags, "Event tap was re-enabled; reconciling pressed keys");
        state.reconcile_after_event_tap_reenabled(flags);
        state.refresh_disable_hotkey_state();
    }
}

impl State {
    fn refresh_disable_hotkey_state(&mut self) {
        let Some(target) = self.disable_hotkey.clone() else {
            return;
        };
        let prev_active = self.disable_hotkey_active;
        self.disable_hotkey_active = self.compute_disable_hotkey_active(&target);
        if self.disable_hotkey_active != prev_active && !self.disable_hotkey_active {
            self.reset(true);
            self.reset_mouse_move_sample_gate();
        }
    }

    #[inline]
    fn reset_mouse_move_sample_gate(&mut self) { self.mouse_move_last_timestamp = None; }

    /// Hands WindowServer work to the actor; never waits.
    fn forward(&mut self, event: TapEvent) {
        if let Err(tokio::sync::mpsc::error::TrySendError::Full(event)) =
            self.forward_tx.try_send(event)
        {
            self.forward_drops += 1;
            debug!(
                ?event,
                drops = self.forward_drops,
                "Input actor is behind; dropping tap work"
            );
        }
    }

    fn on_event(
        &mut self,
        event_type: CGEventType,
        event: &CGEvent,
        proxy: Option<CGEventTapProxy>,
    ) -> bool {
        match event_type {
            ty if ty.0 == gesture::CGS_EVENT_GESTURE || ty.0 == gesture::CGS_EVENT_DOCK_CONTROL => {
                self.native_gesture_forward(ty, event, proxy)
            }
            CGEventType::KeyDown | CGEventType::KeyUp | CGEventType::FlagsChanged => {
                if event::is_rift_synthetic_event(event) {
                    return true;
                }
                if event_type == CGEventType::KeyDown && self.mission_control_active {
                    let keycode = CGEvent::integer_value_field(
                        Some(event),
                        CGEventField::KeyboardEventKeycode,
                    ) as u16;
                    if let Some(input) = super::mission_control::Input::from_keycode(
                        keycode,
                        CGEvent::flags(Some(event)),
                    ) {
                        self.send_overview(super::mission_control::Event::Input(input));
                        return false;
                    }
                }
                self.handle_keyboard_event(event_type, event)
            }
            CGEventType::ScrollWheel if self.mission_control_active => {
                let continuous = CGEvent::integer_value_field(
                    Some(event),
                    CGEventField::ScrollWheelEventIsContinuous,
                ) != 0;
                let (x, y, scale) = if continuous {
                    (
                        CGEventField::ScrollWheelEventPointDeltaAxis2,
                        CGEventField::ScrollWheelEventPointDeltaAxis1,
                        1.0,
                    )
                } else {
                    (
                        CGEventField::ScrollWheelEventDeltaAxis2,
                        CGEventField::ScrollWheelEventDeltaAxis1,
                        16.0,
                    )
                };
                self.send_overview(super::mission_control::Event::Input(
                    super::mission_control::Input::Scroll {
                        point: CGEvent::location(Some(event)),
                        delta: overview_scroll_delta(
                            CGPoint::new(
                                CGEvent::integer_value_field(Some(event), x) as f64 * scale,
                                CGEvent::integer_value_field(Some(event), y) as f64 * scale,
                            ),
                            CGEvent::flags(Some(event)),
                        ),
                    },
                ));
                false
            }
            CGEventType::ScrollWheel => self.native_gesture_forward(event_type, event, proxy),
            CGEventType::MouseMoved => self.on_mouse_moved(event, CGEvent::location(Some(event))),
            CGEventType::LeftMouseDragged | CGEventType::RightMouseDragged => {
                if self.mission_control_active && event_type == CGEventType::LeftMouseDragged {
                    self.send_overview(super::mission_control::Event::Input(
                        super::mission_control::Input::PointerDrag(CGEvent::location(Some(event))),
                    ));
                    // Keep the hardware cursor moving without delivering a drag to apps.
                    CGEvent::set_type(Some(event), CGEventType::MouseMoved);
                    return true;
                }
                if self.mission_control_active {
                    self.send_overview(super::mission_control::Event::Input(
                        super::mission_control::Input::Scroll {
                            point: CGEvent::location(Some(event)),
                            delta: CGPoint::new(
                                CGEvent::integer_value_field(
                                    Some(event),
                                    CGEventField::MouseEventDeltaX,
                                ) as f64,
                                0.0,
                            ),
                        },
                    ));
                    CGEvent::set_type(Some(event), CGEventType::MouseMoved);
                    return true;
                }
                let button = if event_type == CGEventType::LeftMouseDragged {
                    crate::actor::drag::MouseButton::Left
                } else {
                    crate::actor::drag::MouseButton::Right
                };
                let point = self
                    .maybe_horizontal_mouse_warp(event)
                    .unwrap_or_else(|| CGEvent::location(Some(event)));
                let captured = self.captured_button == Some(button);
                if captured || self.native_motion_active.load(Ordering::Acquire) {
                    let publisher = &self.drag_motion_publisher;
                    if publisher.publish(crate::actor::drag::DragMotion { point }) {
                        self.events_tx.send(Event::DragMotionPending(publisher.clone()));
                    }
                }
                !captured
            }
            CGEventType::LeftMouseDown | CGEventType::RightMouseDown => {
                if self.cursor_hidden {
                    self.forward(TapEvent::ShowMouse);
                }
                if self.mission_control_active && event_type == CGEventType::LeftMouseDown {
                    self.send_overview(super::mission_control::Event::Input(
                        super::mission_control::Input::PointerDown(CGEvent::location(Some(event))),
                    ));
                    return false;
                }
                if self.mission_control_active {
                    return false;
                }
                let button = if event_type == CGEventType::LeftMouseDown {
                    crate::actor::drag::MouseButton::Left
                } else {
                    crate::actor::drag::MouseButton::Right
                };
                let action = if button == crate::actor::drag::MouseButton::Left {
                    self.mouse_settings.action1
                } else {
                    self.mouse_settings.action2
                };
                let flag = mouse_modifier_flag(self.mouse_settings.modifier);
                if self.mouse_features_enabled
                    && action != MouseAction::None
                    && CGEvent::flags(Some(event)).contains(flag)
                {
                    self.captured_button = Some(button);
                    self.events_tx.send(Event::ModifierMouseDown {
                        button,
                        point: CGEvent::location(Some(event)),
                        action,
                    });
                    return false;
                }
                if self.stack_line_enabled {
                    let loc = CGEvent::location(Some(event));
                    // Occlusion is a WindowServer query; the actor answered it for
                    // the last sampled move over an indicator.
                    if self.stack_line_rect_hit(loc) && !self.stack_line_occluded {
                        let _ = self.stack_line_tx.try_send(stack_line::Event::MouseDown(loc));
                        return false;
                    }
                }
                true
            }
            CGEventType::LeftMouseUp | CGEventType::RightMouseUp => {
                if event_type == CGEventType::LeftMouseUp && self.mission_control_active {
                    self.send_overview(super::mission_control::Event::Input(
                        super::mission_control::Input::PointerUp(CGEvent::location(Some(event))),
                    ));
                    return false;
                }
                if self.mission_control_active {
                    return false;
                }
                let button = if event_type == CGEventType::LeftMouseUp {
                    crate::actor::drag::MouseButton::Left
                } else {
                    crate::actor::drag::MouseButton::Right
                };
                let captured = self.captured_button == Some(button);
                if self.mouse_features_enabled {
                    self.events_tx.send(Event::MouseUp(button));
                }
                if captured {
                    self.captured_button = None;
                }
                !captured
            }
            _ => true,
        }
    }

    fn send_overview(&self, event: super::mission_control::Event) {
        if let Some(tx) = &self.mission_control_tx {
            tx.send(event);
        }
    }

    fn stack_line_rect_hit(&self, loc: CGPoint) -> bool {
        self.stack_line_hit_rects
            .load()
            .iter()
            .copied()
            .any(|frame| point_hits_indicator_frame(loc, frame))
    }

    /// Handle mouse moves without running the generic mouse/keyboard path.
    ///
    /// Mouse moves are usually the most frequent events delivered to this tap.
    /// In particular, do not read CGEvent flags for every hardware event: the
    /// keyboard and flags-changed events already maintain modifier state, and
    /// the sampled move path below is sufficient as a recovery check.
    fn on_mouse_moved(&mut self, event: &CGEvent, loc: CGPoint) -> bool {
        if !self.event_processing_enabled && !self.mission_control_active {
            return true;
        }
        if self.cursor_hidden {
            self.forward(TapEvent::ShowMouse);
        }
        self.mouse_location = loc;
        if self.mission_control_active {
            self.send_overview(super::mission_control::Event::Input(
                super::mission_control::Input::Move(loc),
            ));
            return true;
        }

        // Recover modifier state at the sampled rate instead of once per raw
        // mouse event. Normal modifier transitions arrive through
        // FlagsChanged; this is only the defensive reconciliation path for
        // events lost while macOS UI interrupts the tap.
        if self.disable_hotkey.is_some() || self.mouse_features_enabled {
            let flags = CGEvent::flags(Some(event));
            if flags != self.current_flags {
                self.current_flags = flags;
                self.reconcile_modifier_keys();
                self.refresh_disable_hotkey_state();
            }
        }

        // The geometry test is in-memory; occlusion and the hit transitions
        // need WindowServer and run on the actor.
        if self.stack_line_enabled {
            let rect_hit = self.stack_line_rect_hit(loc);
            self.forward(TapEvent::StackLineMove { point: loc, rect_hit });
        }

        // Publish positions only. WindowServer hit testing and focus eligibility
        // belong on the reactor, outside the synchronous input callback.
        if self.focus_follows_mouse_config_enabled
            && self.focus_follows_mouse_enabled
            && !self.disable_hotkey_active
            && self.captured_button.is_none()
            && !self.current_flags.contains(mouse_modifier_flag(self.mouse_settings.modifier))
        {
            _ = self.mouse_focus_publisher.publish(&self.events_tx, loc);
        }

        true
    }

    /// Rewrites the event to the far edge and asks the actor to warp the cursor.
    fn maybe_horizontal_mouse_warp(&mut self, event: &CGEvent) -> Option<CGPoint> {
        self.horizontal_mouse_warp?;
        if !self.event_processing_enabled {
            return None;
        }
        let point = CGEvent::location(Some(event));
        let delta = CGEvent::integer_value_field(Some(event), CGEventField::MouseEventDeltaX);
        let target = horizontal_warp_target(&self.warp_screens, point, delta)?;
        self.forward(TapEvent::Warp(target));
        CGEvent::set_location(Some(event), target);
        Some(target)
    }

    #[inline]
    fn admit_mouse_move(&mut self, event: &CGEvent) -> Option<CGPoint> {
        let timestamp = CGEvent::timestamp(Some(event));
        let last_timestamp = self.mouse_move_last_timestamp;
        if last_timestamp.is_some_and(|last| {
            timestamp
                .checked_sub(last)
                .is_some_and(|elapsed| elapsed < self.mouse_move_min_interval_ticks)
        }) {
            return None;
        }
        self.mouse_move_last_timestamp = Some(timestamp);
        Some(CGEvent::location(Some(event)))
    }

    fn handle_keyboard_event(&mut self, event_type: CGEventType, event: &CGEvent) -> bool {
        let key_code_opt = key_code_from_event(event);

        // FlagsChanged must be interpreted using the flags from this event,
        // rather than the previous event's modifier state.
        let flags = CGEvent::flags(Some(event));
        self.current_flags = flags;

        if let Some(key_code) = key_code_opt {
            match event_type {
                CGEventType::KeyDown => {
                    if self.disable_hotkey.as_ref().is_some_and(|key| key.key_code == key_code) {
                        self.note_key_down(key_code);
                    }
                }
                CGEventType::KeyUp => self.note_key_up(key_code),
                CGEventType::FlagsChanged => self.note_flags_changed(key_code),
                _ => {}
            }
        }
        self.refresh_disable_hotkey_state();

        if event_type == CGEventType::KeyDown {
            if let Some(key_code) = key_code_opt {
                let hotkey = Hotkey::new(
                    modifiers_from_flags_with_keys(self.current_flags, &self.pressed_keys),
                    key_code,
                );
                if let Some(commands) =
                    self.hotkeys.get(self.active_mode).and_then(|map| map.get(&hotkey))
                {
                    // A held key generates repeated KeyDown events. Hotkeys
                    // are press-triggered, so dispatching those repeats can
                    // execute a command over and over. This is especially
                    // surprising for workspace_auto_back_and_forth, where
                    // each repeat toggles back to the other workspace.
                    let is_repeat = CGEvent::integer_value_field(
                        Some(event),
                        CGEventField::KeyboardEventAutorepeat,
                    ) != 0;
                    if is_repeat {
                        return false;
                    }
                    let commands = commands.clone();
                    for cmd in &commands {
                        match cmd {
                            WmCommand::Wm(wm_controller::WmCmd::BindingMode(target)) => {
                                self.transition_binding_mode(target);
                            }
                            WmCommand::ReactorCommand(command) => {
                                self.events_tx.send(Event::Command(command.clone()))
                            }
                            _ => self.wm_sender.send(WmEvent::Command(cmd.clone())),
                        }
                    }
                    return false;
                }
            }
        }

        true
    }

    fn active_binding_mode(&self) -> String {
        self.mode_names
            .get(self.active_mode)
            .cloned()
            .unwrap_or_else(|| "default".into())
    }

    fn notify_binding_mode_changed(&self, previous_mode: String) {
        let mode = self.active_binding_mode();
        if mode != previous_mode {
            self.events_tx.send(Event::BindingModeChanged { mode });
        }
    }

    fn install_binding_specs(&mut self, specs: &BindingModeSpecs, rebuild_maps: bool) {
        let previous_mode = self.active_binding_mode();
        self.mode_names = specs.iter().map(|(name, _)| name.clone()).collect();
        self.mode_indices = specs
            .iter()
            .enumerate()
            .map(|(index, (name, _))| (name.clone(), index))
            .collect();
        self.active_mode = 0;
        self.notify_binding_mode_changed(previous_mode);
        if rebuild_maps {
            self.rebuild_binding_maps(specs);
        }
    }

    fn transition_binding_mode(&mut self, target: &str) {
        if let Some(&index) = self.mode_indices.get(target) {
            let previous_mode = self.active_binding_mode();
            self.active_mode = index;
            self.notify_binding_mode_changed(previous_mode);
        }
    }

    fn rebuild_binding_maps(&mut self, specs: &BindingModeSpecs) {
        let mut maps = Vec::with_capacity(specs.len());
        for (mode, bindings) in specs.iter() {
            let mut map: HashMap<Hotkey, Vec<WmCommand>> = HashMap::default();
            for (spec, command) in bindings {
                let Ok(hotkey) = Hotkey::from_str(spec) else {
                    warn!(%spec, %mode, "Skipping hotkey that no longer resolves for current keyboard layout");
                    continue;
                };
                let mut insert = |hotkey| {
                    let entry = map.entry(hotkey).or_default();
                    if !entry.contains(command) {
                        entry.push(command.clone());
                    }
                };
                if hotkey.modifiers.has_generic_modifiers() {
                    for modifiers in hotkey.modifiers.expand_to_specific() {
                        insert(Hotkey::new(modifiers, hotkey.key_code));
                    }
                } else {
                    insert(hotkey);
                }
            }
            maps.push(map);
        }
        trace!("Updated hotkey maps for current keyboard layout: {}", maps.len());
        self.hotkeys = maps;
    }

    fn native_gesture_forward(
        &mut self,
        ty: CGEventType,
        event: &CGEvent,
        proxy: Option<CGEventTapProxy>,
    ) -> bool {
        let gesture_control = &self.gesture_control;
        self.gesture_filter.forward(
            ty,
            event,
            || gesture_control.ownership_guard(),
            |held| {
                if let Some(proxy) = proxy {
                    // The proxy is valid only during this tap callback. Held events
                    // must reach downstream taps before the current event returns.
                    unsafe { CGEvent::tap_post_event(proxy, Some(held)) };
                } else if !cfg!(test) {
                    // Tests feed synthetic gestures to the filter; never post them into the
                    // live session, where the running window manager would receive them.
                    CGEvent::post(CGTapLoc::SessionEventTap, Some(held));
                }
            },
        )
    }
}

fn overview_scroll_delta(delta: CGPoint, flags: CGEventFlags) -> CGPoint {
    if flags.contains(CGEventFlags::MaskShift) {
        CGPoint::new(
            if delta.y.abs() >= delta.x.abs() {
                delta.y
            } else {
                delta.x
            },
            0.0,
        )
    } else {
        delta
    }
}

/// Runs on the event-tap thread while WindowServer waits for the answer: only
/// in-memory work, sends and `try_send`s. See [`State`] for the lock rule.
unsafe extern "C-unwind" fn input_callback(
    proxy: CGEventTapProxy,
    event_type: CGEventType,
    event_ref: core::ptr::NonNull<CGEvent>,
    user_info: *mut std::ffi::c_void,
) -> *mut CGEvent {
    if user_info.is_null() {
        return event_ref.as_ptr();
    }
    let ctx = unsafe { &*(user_info as *const CallbackCtx) };
    let event = unsafe { event_ref.as_ref() };

    let Some(mut guard) = ctx.state.try_lock_for(CALLBACK_LOCK_WAIT) else {
        // A passed-through hotkey reaches the focused app instead. Log the 1st,
        // 2nd, 4th, 8th, ... miss so a live run shows whether that ever happens.
        let misses = CALLBACK_LOCK_MISSES.fetch_add(1, Ordering::Relaxed) + 1;
        if misses.is_power_of_two() {
            warn!(
                misses,
                "Input state lock busy; passed an event through unfiltered"
            );
        }
        return event_ref.as_ptr();
    };
    let state = &mut *guard;

    // Keep rejected high-frequency mouse events out of catch_unwind and the
    // actor/state path. Edge crossings are checked before sampling so a quick
    // movement cannot stall at the edge.
    let mouse_point = if event_type == CGEventType::MouseMoved {
        let warped = if state.mission_control_active {
            None
        } else {
            state.maybe_horizontal_mouse_warp(event)
        };
        match state.admit_mouse_move(event) {
            Some(point) => Some(warped.unwrap_or(point)),
            None => {
                return event_ref.as_ptr();
            }
        }
    } else {
        None
    };

    let was_holding = state.gesture_filter.hold_deadline().is_some();
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        if let Some(point) = mouse_point {
            state.on_mouse_moved(event, point)
        } else {
            state.on_event(event_type, event, Some(proxy))
        }
    }));

    if !was_holding && state.gesture_filter.hold_deadline().is_some() {
        let _ = ctx.recovery_tx.send(Recovery::NativeGestureHeld);
    }
    match result {
        Ok(true) => event_ref.as_ptr(),
        Ok(false) => core::ptr::null_mut(),
        Err(_) => event_ref.as_ptr(),
    }
}

/// Still inside the tap callback: only hand the recovery to the actor loop.
/// Verifying the re-enable and reading live modifier state are WindowServer
/// calls that must not run while WindowServer waits for this callback.
unsafe extern "C-unwind" fn event_tap_reenabled(user_info: *mut std::ffi::c_void) {
    if user_info.is_null() {
        return;
    }
    let ctx = unsafe { &*(user_info as *const CallbackCtx) };
    let _ = ctx
        .recovery_tx
        .send(Recovery::TapDisabled(ctx.tap_generation.load(Ordering::Acquire)));
}

unsafe extern "C-unwind" fn event_tap_invalidated(user_info: *mut std::ffi::c_void) {
    if user_info.is_null() {
        return;
    }
    let ctx = unsafe { &*(user_info as *const CallbackCtx) };
    let _ = ctx.recovery_tx.send(Recovery::TapInvalidated(
        ctx.tap_generation.load(Ordering::Acquire),
    ));
}

impl State {
    fn note_key_down(&mut self, key_code: KeyCode) { self.pressed_keys.insert(key_code); }

    fn note_key_up(&mut self, key_code: KeyCode) { self.pressed_keys.remove(&key_code); }

    fn note_flags_changed(&mut self, key_code: KeyCode) {
        if !is_modifier_key(key_code) {
            return;
        }
        // Use the device-dependent side bit; the family-wide mask cannot
        // distinguish (for example) AltLeft from AltRight.
        if modifier_key_is_active(self.current_flags, key_code) {
            self.pressed_keys.insert(key_code);
        } else {
            self.pressed_keys.remove(&key_code);
        }
    }

    fn reconcile_modifier_keys(&mut self) {
        self.pressed_keys.retain(|key| {
            if is_modifier_key(*key) {
                modifier_key_is_active(self.current_flags, *key)
            } else {
                true // non-modifier keys are not reconciled here
            }
        });
    }

    fn reconcile_after_event_tap_reenabled(&mut self, flags: CGEventFlags) {
        // Any key-up may have occurred while the tap was disabled. Discard the
        // edge-triggered cache and use the authoritative live modifier state.
        self.pressed_keys.clear();
        self.current_flags = flags;
    }

    fn compute_disable_hotkey_active(&self, target: &Hotkey) -> bool {
        let active_mods = modifiers_from_flags_with_keys(self.current_flags, &self.pressed_keys);

        let check_modifier = |left: Modifiers, right: Modifiers| -> bool {
            let target_has_left = target.modifiers.contains(left);
            let target_has_right = target.modifiers.contains(right);
            let active_has_left = active_mods.contains(left);
            let active_has_right = active_mods.contains(right);

            if target_has_left && target_has_right {
                active_has_left || active_has_right
            } else if target_has_left {
                active_has_left
            } else if target_has_right {
                active_has_right
            } else {
                true
            }
        };

        let shift_ok = check_modifier(Modifiers::SHIFT_LEFT, Modifiers::SHIFT_RIGHT);
        let ctrl_ok = check_modifier(Modifiers::CONTROL_LEFT, Modifiers::CONTROL_RIGHT);
        let alt_ok = check_modifier(Modifiers::ALT_LEFT, Modifiers::ALT_RIGHT);
        let meta_ok = check_modifier(Modifiers::META_LEFT, Modifiers::META_RIGHT);

        if !(shift_ok && ctrl_ok && alt_ok && meta_ok) {
            return false;
        }

        self.base_key_active(target.key_code)
    }

    fn base_key_active(&self, key_code: KeyCode) -> bool {
        if is_modifier_key(key_code) {
            modifier_key_is_active(self.current_flags, key_code)
        } else {
            self.pressed_keys.contains(&key_code)
        }
    }

    /// Leaves `captured_button` alone: dropping it here strands a modifier drag without a
    /// DragCancel, so callers that end a drag take it and cancel explicitly.
    fn reset(&mut self, enabled: bool) {
        if enabled {
            self.reset_mouse_sampling();
        }
    }

    #[inline]
    fn reset_mouse_sampling(&mut self) { self.last_stack_line_hit = None; }
}

fn mouse_modifier_flag(modifier: MouseModifier) -> CGEventFlags {
    match modifier {
        MouseModifier::Cmd => CGEventFlags::MaskCommand,
        MouseModifier::Alt => CGEventFlags::MaskAlternate,
        MouseModifier::Shift => CGEventFlags::MaskShift,
        MouseModifier::Ctrl => CGEventFlags::MaskControl,
        MouseModifier::Fn => CGEventFlags::MaskSecondaryFn,
    }
}

#[inline]
fn mouse_move_sampling_profile(low_power_mode: bool) -> u64 {
    let interval_ns = if low_power_mode {
        MOUSE_MOVE_MIN_INTERVAL_NS_LOW_POWER
    } else {
        MOUSE_MOVE_MIN_INTERVAL_NS_NORMAL
    };
    // HID CGEvent timestamps use Mach ticks, unlike Session timestamps.
    // Convert the interval during configuration, not each hardware event.
    #[repr(C)]
    struct Timebase {
        numer: u32,
        denom: u32,
    }
    unsafe extern "C" {
        fn mach_timebase_info(info: *mut Timebase) -> i32;
    }
    let mut timebase = Timebase { numer: 0, denom: 0 };
    let status = unsafe { mach_timebase_info(&mut timebase) };
    assert!(status == 0 && timebase.numer != 0 && timebase.denom != 0);
    interval_in_ticks(interval_ns, timebase.numer, timebase.denom)
}

const WARP_EDGE_THRESHOLD: f64 = 3.0;
const WARP_LANDING_INSET: f64 = 6.0;

fn sort_warp_screens(screens: &mut [CGRect], direction: Option<HorizontalMouseWarp>) {
    screens.sort_by(|a, b| {
        let order = a
            .origin
            .y
            .total_cmp(&b.origin.y)
            .then_with(|| a.origin.x.total_cmp(&b.origin.x));
        if direction == Some(HorizontalMouseWarp::BottomToTop) {
            order.reverse()
        } else {
            order
        }
    });
}

fn horizontal_warp_target(screens: &[CGRect], point: CGPoint, delta_x: i64) -> Option<CGPoint> {
    if delta_x == 0 {
        return None;
    }
    let index = screens.iter().position(|screen| {
        point.x >= screen.origin.x
            && point.x < screen.origin.x + screen.size.width
            && point.y >= screen.origin.y
            && point.y < screen.origin.y + screen.size.height
    })?;
    let source = screens[index];
    let right = delta_x > 0;
    if right && point.x < source.origin.x + source.size.width - WARP_EDGE_THRESHOLD
        || !right && point.x > source.origin.x + WARP_EDGE_THRESHOLD
    {
        return None;
    }
    let target = *screens.get(if right {
        index.checked_add(1)?
    } else {
        index.checked_sub(1)?
    })?;
    // Keep the same distance from the top; a shorter target has no crossing below its bottom.
    let y = target.origin.y + point.y - source.origin.y;
    if y < target.origin.y || y >= target.origin.y + target.size.height {
        return None;
    }
    // Inset the landing point so a following reverse movement cannot immediately warp back.
    let inset = WARP_LANDING_INSET.min((target.size.width / 2.0).max(0.0));
    Some(CGPoint::new(
        if right {
            target.origin.x + inset
        } else {
            target.origin.x + target.size.width - inset
        },
        y,
    ))
}

fn interval_in_ticks(nanoseconds: u64, numer: u32, denom: u32) -> u64 {
    (nanoseconds * u64::from(denom)).div_ceil(u64::from(numer)).max(1)
}

// AX drag acquisition reads live HID button state. Only an active drag needs release
// events; dragged events add no information. KeyUp releases keyed disable shortcuts.
fn build_event_mask(
    key_down_enabled: bool,
    flags_enabled: bool,
    mouse_move_enabled: bool,
    buttons_enabled: bool,
    release_enabled: bool,
    key_up_enabled: bool,
) -> CGEventMask {
    let mut mask = 0;
    if buttons_enabled {
        for ty in [CGEventType::LeftMouseDown, CGEventType::RightMouseDown] {
            mask |= 1u64 << ty.0;
        }
    }
    if release_enabled {
        mask |= (1u64 << CGEventType::LeftMouseUp.0) | (1u64 << CGEventType::RightMouseUp.0);
    }
    if key_up_enabled {
        mask |= 1u64 << CGEventType::KeyUp.0;
    }
    if mouse_move_enabled {
        mask |= 1u64 << CGEventType::MouseMoved.0;
    }
    if key_down_enabled {
        mask |= 1u64 << CGEventType::KeyDown.0;
    }
    if flags_enabled {
        mask |= 1u64 << CGEventType::FlagsChanged.0;
    }
    mask
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_hold_timeout_does_not_reject_a_slow_physical_stroke() {
        let (input, _, _) = input();
        *input.gesture_control.ownership_guard() = gesture::Ownership {
            session: 1,
            owner: gesture::Owner::Undecided,
            consume: true,
            touching: true,
            dock_owner: None,
        };
        let event = |phase| {
            let event = CGEvent::new_scroll_wheel_event2(
                None,
                objc2_core_graphics::CGScrollEventUnit::Pixel,
                2,
                0,
                0,
                0,
            )
            .unwrap();
            CGEvent::set_integer_value_field(
                Some(&event),
                CGEventField::ScrollWheelEventScrollPhase,
                phase,
            );
            event
        };
        assert!(!input.state.lock().native_gesture_forward(
            CGEventType::ScrollWheel,
            &event(1),
            None
        ));
        std::thread::sleep(Duration::from_millis(70));
        input
            .state
            .lock()
            .native_gesture_forward(CGEventType::ScrollWheel, &event(2), None);
        assert_eq!(
            input.gesture_control.ownership_guard().owner,
            gesture::Owner::Undecided,
            "native delivery timeout must not impose a minimum recognition speed"
        );
        input.gesture_control.ownership_guard().owner = gesture::Owner::Rift;
        assert!(!input.state.lock().native_gesture_forward(
            CGEventType::ScrollWheel,
            &event(2),
            None
        ));
    }

    #[test]
    fn horizontal_warp_geometry() {
        let rect =
            |x, y, w, h| CGRect::new(CGPoint::new(x, y), objc2_core_foundation::CGSize::new(w, h));
        let top = rect(100.0, 0.0, 100.0, 100.0);
        let middle = rect(-50.0, 100.0, 80.0, 70.0);
        let bottom = rect(300.0, 170.0, 100.0, 100.0);
        let mut screens = vec![bottom, top, middle];
        sort_warp_screens(&mut screens, Some(HorizontalMouseWarp::TopToBottom));
        assert_eq!(
            horizontal_warp_target(&screens, CGPoint::new(198.0, 20.0), 1),
            Some(CGPoint::new(-44.0, 120.0))
        );
        assert_eq!(
            horizontal_warp_target(&screens, CGPoint::new(-49.0, 120.0), -1),
            Some(CGPoint::new(194.0, 20.0))
        );
        assert_eq!(
            horizontal_warp_target(&screens, CGPoint::new(101.0, 20.0), -1),
            None
        );
        assert_eq!(
            horizontal_warp_target(&screens, CGPoint::new(398.0, 190.0), 1),
            None
        );
        assert_eq!(
            horizontal_warp_target(&screens, CGPoint::new(101.0, 20.0), 1),
            None
        );
        assert_eq!(
            horizontal_warp_target(&screens, CGPoint::new(198.0, 20.0), -1),
            None
        );
        assert_eq!(
            horizontal_warp_target(&screens, CGPoint::new(198.0, 90.0), 1),
            None
        );
        assert_eq!(
            horizontal_warp_target(&screens, CGPoint::new(-49.0, 120.0), 1),
            None
        );
        assert_eq!(
            horizontal_warp_target(&screens, CGPoint::new(28.0, 120.0), 1),
            Some(CGPoint::new(306.0, 190.0))
        );
        assert_eq!(
            horizontal_warp_target(&screens, CGPoint::new(-44.0, 120.0), -1),
            None
        );
        sort_warp_screens(&mut screens, Some(HorizontalMouseWarp::BottomToTop));
        assert_eq!(
            horizontal_warp_target(&screens, CGPoint::new(398.0, 190.0), 1),
            Some(CGPoint::new(-44.0, 120.0))
        );
    }

    #[test]
    fn horizontal_warp_shared_boundary_belongs_to_lower_display() {
        let top = CGRect::new(
            CGPoint::new(0.0, 0.0),
            objc2_core_foundation::CGSize::new(100.0, 900.0),
        );
        let bottom = CGRect::new(CGPoint::new(0.0, 900.0), top.size);
        assert_eq!(
            horizontal_warp_target(&[top, bottom], CGPoint::new(0.0, 900.0), -1),
            Some(CGPoint::new(94.0, 0.0))
        );
        assert_eq!(
            horizontal_warp_target(&[bottom, top], CGPoint::new(99.0, 900.0), 1),
            Some(CGPoint::new(6.0, 0.0))
        );
        assert_eq!(
            horizontal_warp_target(&[top, bottom], CGPoint::new(100.0, 900.0), -1),
            None
        );
    }

    #[test]
    fn disabled_horizontal_warp_is_a_no_op() {
        let (input, _, _) = input();
        let event = CGEvent::new(None).unwrap();
        let mut state = input.state.lock();
        assert_eq!(state.maybe_horizontal_mouse_warp(&event), None);
        assert_eq!(state.forward_drops, 0);
    }

    #[test]
    fn hid_mouse_throttle_uses_mach_ticks_instead_of_nanoseconds() {
        let (input, _, _) = input();
        // This Apple Silicon timebase is 125/3 ns per tick. Sixteen milliseconds
        // is 384,000 ticks; interpreting nanoseconds as ticks would delay FFM.
        let interval = interval_in_ticks(MOUSE_MOVE_MIN_INTERVAL_NS_NORMAL, 125, 3);
        assert_eq!(interval, 384_000);
        assert_eq!(
            interval_in_ticks(MOUSE_MOVE_MIN_INTERVAL_NS_LOW_POWER, 125, 3),
            768_000
        );
        assert_eq!(
            interval_in_ticks(MOUSE_MOVE_MIN_INTERVAL_NS_NORMAL, 1, 1),
            16_000_000
        );
        input.state.lock().mouse_move_min_interval_ticks = interval;
        let event = CGEvent::new_mouse_event(
            None,
            CGEventType::MouseMoved,
            CGPoint::new(20.0, 30.0),
            objc2_core_graphics::CGMouseButton::Left,
        )
        .unwrap();
        let start = 1_000_000_000;
        CGEvent::set_timestamp(Some(&event), start);
        assert_eq!(
            input.state.lock().admit_mouse_move(&event),
            Some(CGPoint::new(20.0, 30.0))
        );
        CGEvent::set_timestamp(Some(&event), start + interval - 1);
        assert!(input.state.lock().admit_mouse_move(&event).is_none());
        // Moving elsewhere does not bypass the time-based sample gate.
        CGEvent::set_location(Some(&event), CGPoint::new(150.0, 30.0));
        assert!(input.state.lock().admit_mouse_move(&event).is_none());
        CGEvent::set_timestamp(Some(&event), start + interval);
        assert_eq!(
            input.state.lock().admit_mouse_move(&event),
            Some(CGPoint::new(150.0, 30.0))
        );
        // An older timestamp (e.g. switching event sources) resets the gate
        // rather than rejecting hardware input until the old clock catches up.
        CGEvent::set_timestamp(Some(&event), start - interval);
        assert!(input.state.lock().admit_mouse_move(&event).is_some());
        CGEvent::set_timestamp(Some(&event), start - 1);
        assert!(input.state.lock().admit_mouse_move(&event).is_none());
        CGEvent::set_timestamp(Some(&event), start);
        assert!(input.state.lock().admit_mouse_move(&event).is_some());
    }

    #[test]
    fn overview_passes_sampled_and_skipped_mouse_motion_to_cursor() {
        let (input, mut wm_rx, mut native_rx) = input();
        let (tx, mut rx) = actor::channel();
        input.state.lock().mission_control_tx = Some(tx);
        input.state.lock().mission_control_active = true;
        input.state.lock().mouse_move_min_interval_ticks = 100;
        let (recovery_tx, _) = tokio::sync::mpsc::unbounded_channel();
        let mut ctx = CallbackCtx {
            state: input.state.clone(),
            recovery_tx,
            tap_generation: AtomicU64::new(0),
        };
        let event = CGEvent::new(None).unwrap();
        CGEvent::set_type(Some(&event), CGEventType::MouseMoved);
        CGEvent::set_timestamp(Some(&event), 1000);
        CGEvent::set_location(Some(&event), CGPoint::new(30.0, 40.0));
        let event_ptr = core::ptr::NonNull::from(&*event);
        let context = (&mut ctx as *mut CallbackCtx).cast();
        let result = unsafe {
            input_callback(
                core::ptr::null_mut(),
                CGEventType::MouseMoved,
                event_ptr,
                context,
            )
        };
        assert_eq!(result, event_ptr.as_ptr());
        assert!(matches!(
            rx.try_recv().unwrap().1,
            super::super::mission_control::Event::Input(
                super::super::mission_control::Input::Move(_)
            )
        ));
        CGEvent::set_timestamp(Some(&event), 1001);
        let result = unsafe {
            input_callback(
                core::ptr::null_mut(),
                CGEventType::MouseMoved,
                event_ptr,
                context,
            )
        };
        assert_eq!(result, event_ptr.as_ptr());
        assert!(rx.try_recv().is_err());
        assert!(wm_rx.try_recv().is_err());
        assert!(native_rx.try_recv().is_err());
    }

    #[test]
    fn overview_pointer_sequence_never_enters_native_drag_path() {
        let (input, mut wm_rx, mut native_rx) = input();
        let (tx, mut rx) = actor::channel();
        input.state.lock().mission_control_tx = Some(tx);
        input.state.lock().mission_control_active = true;
        let event = CGEvent::new(None).unwrap();
        CGEvent::set_location(Some(&event), CGPoint::new(30.0, 40.0));
        for ty in [
            CGEventType::LeftMouseDown,
            CGEventType::LeftMouseDragged,
            CGEventType::LeftMouseUp,
        ] {
            assert_eq!(
                input.state.lock().on_event(ty, &event, None),
                ty == CGEventType::LeftMouseDragged
            );
            if ty == CGEventType::LeftMouseDragged {
                assert_eq!(CGEvent::r#type(Some(&event)), CGEventType::MouseMoved);
            }
        }
        assert!(matches!(
            rx.try_recv().unwrap().1,
            super::super::mission_control::Event::Input(
                super::super::mission_control::Input::PointerDown(_)
            )
        ));
        assert!(matches!(
            rx.try_recv().unwrap().1,
            super::super::mission_control::Event::Input(
                super::super::mission_control::Input::PointerDrag(_)
            )
        ));
        assert!(matches!(
            rx.try_recv().unwrap().1,
            super::super::mission_control::Event::Input(
                super::super::mission_control::Input::PointerUp(_)
            )
        ));
        assert!(rx.try_recv().is_err());
        assert!(native_rx.try_recv().is_err());
        assert!(wm_rx.try_recv().is_err());
    }

    #[test]
    fn overview_right_drag_pans_without_native_drag() {
        let (input, mut wm_rx, mut native_rx) = input();
        let (tx, mut rx) = actor::channel();
        input.state.lock().mission_control_tx = Some(tx);
        input.state.lock().mission_control_active = true;
        let event = CGEvent::new_mouse_event(
            None,
            CGEventType::RightMouseDragged,
            CGPoint::new(30.0, 40.0),
            objc2_core_graphics::CGMouseButton::Right,
        )
        .unwrap();
        CGEvent::set_location(Some(&event), CGPoint::new(30.0, 40.0));
        CGEvent::set_integer_value_field(Some(&event), CGEventField::MouseEventDeltaX, 25);
        for ty in [
            CGEventType::RightMouseDown,
            CGEventType::RightMouseDragged,
            CGEventType::RightMouseUp,
        ] {
            assert_eq!(
                input.state.lock().on_event(ty, &event, None),
                ty == CGEventType::RightMouseDragged
            );
            if ty == CGEventType::RightMouseDragged {
                assert_eq!(CGEvent::r#type(Some(&event)), CGEventType::MouseMoved);
            }
        }
        let (
            _,
            super::super::mission_control::Event::Input(
                super::super::mission_control::Input::Scroll { delta, .. },
            ),
        ) = rx.try_recv().unwrap()
        else {
            panic!("horizontal Overview pan")
        };
        assert_eq!(delta, CGPoint::new(25.0, 0.0));
        assert!(rx.try_recv().is_err());
        assert!(native_rx.try_recv().is_err());
        assert!(wm_rx.try_recv().is_err());
    }

    #[test]
    fn shift_wheel_pans_strip_without_changing_workspace_axis() {
        let (input, _, _) = input();
        let (tx, mut rx) = actor::channel();
        input.state.lock().mission_control_tx = Some(tx);
        input.state.lock().mission_control_active = true;
        let event = CGEvent::new_scroll_wheel_event2(
            None,
            objc2_core_graphics::CGScrollEventUnit::Pixel,
            2,
            -60,
            0,
            0,
        )
        .unwrap();
        CGEvent::set_flags(Some(&event), CGEventFlags::MaskShift);
        assert!(!input.state.lock().on_event(CGEventType::ScrollWheel, &event, None));
        assert!(
            matches!(rx.try_recv().unwrap().1, super::super::mission_control::Event::Input(super::super::mission_control::Input::Scroll { delta, .. }) if delta == CGPoint::new(-60.0, 0.0))
        );
        assert_eq!(
            overview_scroll_delta(CGPoint::new(-30.0, 0.0), CGEventFlags::MaskShift),
            CGPoint::new(-30.0, 0.0)
        );
        assert_eq!(
            overview_scroll_delta(CGPoint::new(0.0, -60.0), CGEventFlags::empty()),
            CGPoint::new(0.0, -60.0)
        );
    }

    #[test]
    fn overview_consumes_scroll_and_routes_point_deltas() {
        let (input, _, _) = input();
        let (tx, mut rx) = actor::channel();
        input.state.lock().mission_control_tx = Some(tx);
        input.state.lock().mission_control_active = true;
        let event = CGEvent::new_scroll_wheel_event2(
            None,
            objc2_core_graphics::CGScrollEventUnit::Pixel,
            2,
            -60,
            12,
            0,
        )
        .unwrap();
        CGEvent::set_location(Some(&event), CGPoint::new(30.0, 40.0));
        assert!(!input.state.lock().on_event(CGEventType::ScrollWheel, &event, None));
        let (
            _,
            super::super::mission_control::Event::Input(
                super::super::mission_control::Input::Scroll { point, delta },
            ),
        ) = rx.try_recv().unwrap()
        else {
            panic!("expected Overview scroll")
        };
        assert_eq!(point, CGPoint::new(30.0, 40.0));
        assert_eq!(delta, CGPoint::new(12.0, -60.0));
        input.state.lock().mission_control_active = false;
        assert!(input.state.lock().on_event(CGEventType::ScrollWheel, &event, None));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn absent_overview_sender_safely_ignores_commands() {
        let (input, _, _) = input();
        input.state.lock().mission_control_tx.take();
        input.state.lock().send_overview(super::super::mission_control::Event::ShowAll);
        assert!(!input.state.lock().mission_control_active);
    }

    #[test]
    fn input_runs_on_cf_run_loop_without_a_tokio_runtime() {
        let (input, _, _) = input();
        // The closed request channel makes the actor exit after polling select.
        // Even disabled select branches used to construct a Tokio sleep here.
        crate::sys::executor::Executor::run(input.run());
    }

    fn test_config() -> Config {
        let mut config = Config::default();
        config.settings.gestures.enabled = false;
        config.settings.layout.scrolling.gestures.enabled = false;
        config.settings.focus_follows_mouse = false;
        config.settings.focus_follows_mouse_disable_hotkey = None;
        config.settings.mouse_hides_on_focus = false;
        config.settings.ui.stack_line.enabled = false;
        config
    }

    fn input() -> (Input, actor::Receiver<WmEvent>, actor::Receiver<Event>) {
        input_with(test_config())
    }

    fn input_with(config: Config) -> (Input, actor::Receiver<WmEvent>, actor::Receiver<Event>) {
        let (events_tx, events_rx) = actor::channel();
        let (_, requests_rx) = actor::channel();
        let (wm_tx, wm_rx) = actor::channel();
        let (stack_tx, _) = actor::channel();
        let (mc_tx, _) = actor::channel();
        (
            Input::new(
                config,
                events_tx,
                requests_rx,
                wm_tx,
                stack_tx,
                Some(mc_tx),
                stack_line::new_shared_hit_rects(),
                Arc::default(),
            ),
            wm_rx,
            events_rx,
        )
    }

    /// Stands in for `EventTap` in unit tests: keeps what the actor asked for
    /// instead of inserting a real tap into the live HID event chain.
    pub(super) struct RecordedTap {
        mask: CGEventMask,
        placement: CGTapPlace,
        enabled: Cell<bool>,
    }

    impl RecordedTap {
        /// Same signature as `EventTap::new`, so the actor's call is unchanged.
        pub(super) unsafe fn new(
            location: CGTapLoc,
            options: CGTapOpt,
            mask: CGEventMask,
            placement: CGTapPlace,
            _callback: crate::sys::event_tap::TapCallback,
            _user_info: *mut std::ffi::c_void,
            _drop_ctx: Option<unsafe fn(*mut std::ffi::c_void)>,
            _reenabled: crate::sys::event_tap::TapReenabledCallback,
            _invalidated: crate::sys::event_tap::TapInvalidatedCallback,
            _thread: Option<&Arc<TapThread>>,
        ) -> Option<Self> {
            assert_eq!(location, CGTapLoc::HIDEventTap);
            assert_eq!(options, CGTapOpt::Default);
            Some(Self {
                mask,
                placement,
                enabled: Cell::new(true),
            })
        }

        pub(super) fn is_enabled(&self) -> bool { self.enabled.get() }

        pub(super) fn placement(&self) -> CGTapPlace { self.placement }
    }

    /// (generation, mask, placement) of the installed tap.
    fn installed_tap(input: &Input) -> Option<(u64, CGEventMask, CGTapPlace)> {
        let tap = input.tap.borrow();
        tap.as_ref().map(|tap| (input.tap_generation.get(), tap.mask, tap.placement))
    }

    const RELEASES: CGEventMask =
        (1u64 << CGEventType::LeftMouseUp.0) | (1u64 << CGEventType::RightMouseUp.0);

    #[test]
    fn first_tap_and_every_rebuild_use_the_configured_placement() {
        let mut config = test_config();
        config.settings.event_tap_placement = EventTapPlacement::Tail;
        config.settings.drag_drop.enabled = false;
        let (input, _, _) = input_with(config);
        input.rebuild_event_tap_mask_if_needed();
        assert_eq!(installed_tap(&input), None, "no tap while nothing is masked");

        // First tap: what `run()` does on start once event processing is on.
        input.state.lock().event_processing_enabled = true;
        input.rebuild_event_tap_mask_if_needed();
        let tail = CGTapPlace::TailAppendEventTap;
        assert_eq!(installed_tap(&input), Some((1, RELEASES, tail)));

        // Nothing changed: the tap stays.
        input.rebuild_event_tap_mask_if_needed();
        assert_eq!(installed_tap(&input).unwrap().0, 1);

        // Mask change (focus-follows-mouse adds mouse moves).
        input.state.lock().focus_follows_mouse_config_enabled = true;
        input.rebuild_event_tap_mask_if_needed();
        let moves = RELEASES | (1u64 << CGEventType::MouseMoved.0);
        assert_eq!(installed_tap(&input), Some((2, moves, tail)));

        // Mask change on the request path (Mission Control rebuilds the tap).
        input.on_request(Request::SetMissionControlActive(true));
        let (generation, mask, placement) = installed_tap(&input).unwrap();
        assert_eq!((generation, placement), (3, tail));
        assert_ne!(mask & (1u64 << CGEventType::ScrollWheel.0), 0);
        input.on_request(Request::SetMissionControlActive(false));
        assert_eq!(installed_tap(&input), Some((4, moves, tail)));

        // Mach port invalidated: recreated at the same place.
        input.rebuild_invalidated_event_tap(4);
        assert_eq!(installed_tap(&input), Some((5, moves, tail)));

        // WindowServer disabled it and the re-enable did not take: recreated.
        input.tap.borrow().as_ref().unwrap().enabled.set(false);
        input.on_tap_disabled(5);
        assert_eq!(installed_tap(&input), Some((6, moves, tail)));

        // Lost tap: the actor loop's retry path.
        input.tap.borrow_mut().take();
        input.rebuild_event_tap_mask_if_needed();
        assert_eq!(installed_tap(&input), Some((7, moves, tail)));
    }

    #[test]
    fn placement_change_on_reload_rebuilds_the_tap_with_the_same_mask() {
        let mut config = test_config();
        config.settings.drag_drop.enabled = false;
        let (input, _, _) = input_with(config.clone());
        input.state.lock().event_processing_enabled = true;
        input.rebuild_event_tap_mask_if_needed();
        let head = CGTapPlace::HeadInsertEventTap;
        let tail = CGTapPlace::TailAppendEventTap;
        assert_eq!(installed_tap(&input), Some((1, RELEASES, head)));

        // A reload that changes nothing keeps the tap.
        input.on_request(Request::ConfigUpdated(config.clone()));
        assert_eq!(installed_tap(&input), Some((1, RELEASES, head)));

        config.settings.event_tap_placement = EventTapPlacement::Tail;
        input.on_request(Request::ConfigUpdated(config.clone()));
        assert_eq!(installed_tap(&input), Some((2, RELEASES, tail)));
        input.on_request(Request::ConfigUpdated(config.clone()));
        assert_eq!(installed_tap(&input), Some((2, RELEASES, tail)));

        config.settings.event_tap_placement = EventTapPlacement::Head;
        input.on_request(Request::ConfigUpdated(config));
        assert_eq!(installed_tap(&input), Some((3, RELEASES, head)));
    }

    #[test]
    fn placement_change_while_nothing_is_masked_creates_no_tap() {
        let mut config = test_config();
        config.settings.drag_drop.enabled = false;
        let (input, _, _) = input_with(config.clone());
        config.settings.event_tap_placement = EventTapPlacement::Tail;
        input.on_request(Request::ConfigUpdated(config));
        assert_eq!(installed_tap(&input), None);
        input.state.lock().event_processing_enabled = true;
        input.rebuild_event_tap_mask_if_needed();
        assert_eq!(
            installed_tap(&input),
            Some((1, RELEASES, CGTapPlace::TailAppendEventTap))
        );
    }

    #[test]
    fn mask_tracks_mouse_feature_enablement() {
        let (input, _, _) = input();
        assert!(input.state.lock().hotkeys.is_empty());
        assert_eq!(input.desired_event_mask(&input.state.lock()), 0);
        input.state.lock().mouse_features_enabled = false;
        input.state.lock().event_processing_enabled = true;
        let stable_release_mask =
            (1u64 << CGEventType::LeftMouseUp.0) | (1u64 << CGEventType::RightMouseUp.0);
        assert_eq!(
            input.desired_event_mask(&input.state.lock()),
            stable_release_mask
        );
        input.state.lock().focus_follows_mouse_config_enabled = true;
        assert_eq!(
            input.desired_event_mask(&input.state.lock()),
            stable_release_mask | (1u64 << CGEventType::MouseMoved.0)
        );
        input.state.lock().mouse_settings.action2 = MouseAction::Move;
        input.state.lock().mouse_features_enabled = true;
        let mouse_mask = input.desired_event_mask(&input.state.lock());
        assert_ne!(mouse_mask & (1u64 << CGEventType::LeftMouseDown.0), 0);
        assert_ne!(mouse_mask & (1u64 << CGEventType::RightMouseDown.0), 0);
        assert_ne!(mouse_mask & (1u64 << CGEventType::LeftMouseDragged.0), 0);
        assert_ne!(mouse_mask & (1u64 << CGEventType::RightMouseDragged.0), 0);
        input.state.lock().mouse_settings.action2 = MouseAction::None;
        let left_only_mask = input.desired_event_mask(&input.state.lock());
        assert_ne!(left_only_mask & (1u64 << CGEventType::LeftMouseDown.0), 0);
        assert_eq!(left_only_mask & (1u64 << CGEventType::RightMouseDown.0), 0);
        assert_eq!(left_only_mask & (1u64 << CGEventType::RightMouseDragged.0), 0);
        input.state.lock().focus_follows_mouse_config_enabled = false;
        input.state.lock().mouse_features_enabled = false;
        input.state.lock().mission_control_active = true;
        let mask = input.desired_event_mask(&input.state.lock());
        assert_ne!(mask & (1u64 << CGEventType::ScrollWheel.0), 0);
        assert_ne!(mask & (1u64 << CGEventType::KeyDown.0), 0);
        assert_eq!(mask & (1u64 << CGEventType::KeyUp.0), 0);
        assert_ne!(mask & (1u64 << CGEventType::RightMouseDown.0), 0);
        assert_ne!(mask & (1u64 << CGEventType::LeftMouseDragged.0), 0);
        input.state.lock().mission_control_active = false;
        input.state.lock().disable_hotkey = Some(Hotkey::new(Modifiers::empty(), KeyCode::KeyA));
        assert_ne!(
            input.desired_event_mask(&input.state.lock()) & (1u64 << CGEventType::KeyUp.0),
            0
        );
        input.state.lock().disable_hotkey =
            Some(Hotkey::new(Modifiers::empty(), KeyCode::ShiftLeft));
        assert_eq!(
            input.desired_event_mask(&input.state.lock()) & (1u64 << CGEventType::KeyUp.0),
            0
        );
    }

    #[test]
    fn hotkey_maps_are_deferred_until_app_events_are_registered() {
        std::thread::spawn(|| {
            let (input, _, _) = input();
            assert!(!input.hotkeys_active.get());
            assert!(input.state.lock().hotkeys.is_empty());

            input.hotkeys_active.set(true);
            input.state.lock().rebuild_binding_maps(&input.binding_mode_specs.borrow());

            assert!(!input.state.lock().hotkeys.is_empty());
            let key_mask = (1u64 << CGEventType::KeyDown.0) | (1u64 << CGEventType::FlagsChanged.0);
            assert_eq!(
                input.desired_event_mask(&input.state.lock()) & key_mask,
                key_mask
            );
        })
        .join()
        .unwrap();
    }

    #[test]
    fn hotkeys_suppress_repeats_but_do_not_intercept_rift_synthetic_keys() {
        let (input, mut wm_rx, _) = input();
        input.hotkeys_active.set(true);
        input.state.lock().rebuild_binding_maps(&input.binding_mode_specs.borrow());
        input
            .state
            .lock()
            .hotkeys
            .get_mut(0)
            .unwrap()
            .insert(Hotkey::new(Modifiers::empty(), KeyCode::KeyA), vec![
                WmCommand::Wm(wm_controller::WmCmd::ReloadConfig),
            ]);
        let event = CGEvent::new_keyboard_event(None, 0, true).unwrap();
        CGEvent::set_flags(Some(&event), CGEventFlags::empty());
        assert!(!input.state.lock().on_event(CGEventType::KeyDown, &event, None));
        assert!(wm_rx.try_recv().unwrap().0.is_none());
        CGEvent::set_integer_value_field(Some(&event), CGEventField::KeyboardEventAutorepeat, 1);
        assert!(!input.state.lock().on_event(CGEventType::KeyDown, &event, None));
        assert!(wm_rx.try_recv().is_err());
        CGEvent::set_integer_value_field(
            Some(&event),
            CGEventField::EventSourceUserData,
            0x5249_4654,
        );
        assert!(input.state.lock().on_event(CGEventType::KeyDown, &event, None));
        assert!(wm_rx.try_recv().is_err());
    }

    #[test]
    fn binding_mode_changes_notify_and_reload_resets_to_default() {
        let (input, _, mut events) = input();
        let specs = vec![("default".into(), vec![]), ("resize".into(), vec![])];
        input.install_binding_specs(&mut input.state.lock(), specs.clone());
        assert!(events.try_recv().is_err());
        input.state.lock().transition_binding_mode("resize");
        assert!(
            matches!(events.try_recv().unwrap().1, Event::BindingModeChanged { mode } if mode == "resize")
        );
        input.state.lock().transition_binding_mode("resize");
        input.state.lock().transition_binding_mode("missing");
        assert!(events.try_recv().is_err());
        input.install_binding_specs(&mut input.state.lock(), specs);
        assert!(
            matches!(events.try_recv().unwrap().1, Event::BindingModeChanged { mode } if mode == "default")
        );
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn active_mode_replacement_unbinds_default_keys_and_mode_switches_are_immediate() {
        let (input, mut wm_rx, _) = input();
        let config = Config::parse(
            r#"
                [settings]
                [keys]
                "A" = "reload_config"
                "B" = { binding_mode = "resize" }
                [binding_modes.resize]
                "Escape" = { binding_mode = "default" }
            "#,
        )
        .unwrap();
        input.install_binding_specs(&mut input.state.lock(), config.binding_mode_specs);
        input.hotkeys_active.set(true);
        input.state.lock().rebuild_binding_maps(&input.binding_mode_specs.borrow());

        let b = CGEvent::new_keyboard_event(None, 11, true).unwrap();
        assert!(!input.state.lock().on_event(CGEventType::KeyDown, &b, None));
        assert_eq!(input.state.lock().active_mode, 1);

        let a = CGEvent::new_keyboard_event(None, 0, true).unwrap();
        assert!(input.state.lock().on_event(CGEventType::KeyDown, &a, None));
        assert!(wm_rx.try_recv().is_err());

        CGEvent::set_integer_value_field(Some(&b), CGEventField::KeyboardEventAutorepeat, 1);
        assert!(input.state.lock().on_event(CGEventType::KeyDown, &b, None));
        assert_eq!(input.state.lock().active_mode, 1);

        let escape = CGEvent::new_keyboard_event(None, 53, true).unwrap();
        assert!(!input.state.lock().on_event(CGEventType::KeyDown, &escape, None));
        assert_eq!(input.state.lock().active_mode, 0);

        assert!(!input.state.lock().on_event(CGEventType::KeyDown, &a, None));
        assert!(matches!(
            wm_rx.try_recv().unwrap().1,
            WmEvent::Command(WmCommand::Wm(wm_controller::WmCmd::ReloadConfig))
        ));
        assert!(wm_rx.try_recv().is_err());
    }

    #[test]
    fn normal_command_after_binding_mode_transition_still_runs() {
        let (input, mut wm_rx, _) = input();
        input.install_binding_specs(&mut input.state.lock(), vec![
            ("default".into(), vec![(
                "A".into(),
                WmCommand::Wm(wm_controller::WmCmd::BindingMode("other".into())),
            )]),
            ("other".into(), vec![]),
        ]);
        input.hotkeys_active.set(true);
        input.state.lock().rebuild_binding_maps(&input.binding_mode_specs.borrow());
        input.state.lock().hotkeys[0]
            .get_mut(&Hotkey::new(Modifiers::empty(), KeyCode::KeyA))
            .unwrap()
            .push(WmCommand::Wm(wm_controller::WmCmd::ReloadConfig));

        let a = CGEvent::new_keyboard_event(None, 0, true).unwrap();
        assert!(!input.state.lock().on_event(CGEventType::KeyDown, &a, None));
        assert_eq!(input.state.lock().active_mode, 1);
        assert!(matches!(
            wm_rx.try_recv().unwrap().1,
            WmEvent::Command(WmCommand::Wm(wm_controller::WmCmd::ReloadConfig))
        ));
    }

    #[test]
    fn layout_rebuild_preserves_active_mode_and_rebuilds_each_map() {
        let (input, _, _) = input();
        input.install_binding_specs(&mut input.state.lock(), vec![
            ("default".into(), vec![(
                "Ctrl + A".into(),
                WmCommand::Wm(wm_controller::WmCmd::ReloadConfig),
            )]),
            ("other".into(), vec![(
                "Ctrl + B".into(),
                WmCommand::Wm(wm_controller::WmCmd::ReloadConfig),
            )]),
        ]);
        input.hotkeys_active.set(true);
        input.state.lock().active_mode = 1;
        input.state.lock().rebuild_binding_maps(&input.binding_mode_specs.borrow());
        assert_eq!(input.state.lock().active_mode, 1);
        let state = input.state.lock();
        let maps = &state.hotkeys;
        assert!(!maps[0].is_empty());
        assert!(!maps[1].is_empty());
    }

    #[test]
    fn generic_modifiers_expand_inside_each_mode_and_replacement_resets_to_default() {
        let (input, _, _) = input();
        let specs = vec![
            ("default".into(), vec![(
                "Ctrl + A".into(),
                WmCommand::Wm(wm_controller::WmCmd::ReloadConfig),
            )]),
            ("other".into(), vec![(
                "Alt + B".into(),
                WmCommand::Wm(wm_controller::WmCmd::ReloadConfig),
            )]),
        ];
        input.install_binding_specs(&mut input.state.lock(), specs.clone());
        input.hotkeys_active.set(true);
        input.state.lock().rebuild_binding_maps(&input.binding_mode_specs.borrow());
        input.state.lock().active_mode = 1;
        input.install_binding_specs(&mut input.state.lock(), specs);
        assert_eq!(input.state.lock().active_mode, 0);
        let state = input.state.lock();
        let maps = &state.hotkeys;
        assert!(maps[0].keys().any(|key| {
            key.key_code == KeyCode::KeyA && !key.modifiers.has_generic_modifiers()
        }));
        assert!(maps[1].keys().any(|key| {
            key.key_code == KeyCode::KeyB && !key.modifiers.has_generic_modifiers()
        }));
    }

    #[test]
    fn releases_are_always_forwarded_when_mouse_features_are_enabled() {
        let (input, _, mut events_rx) = input();
        input.state.lock().mouse_features_enabled = false;
        let event = CGEvent::new_mouse_event(
            None,
            CGEventType::LeftMouseUp,
            CGPoint::new(20.0, 30.0),
            objc2_core_graphics::CGMouseButton::Left,
        )
        .unwrap();
        assert!(input.state.lock().on_event(CGEventType::LeftMouseUp, &event, None));
        assert!(events_rx.try_recv().is_err());
        input.state.lock().mouse_features_enabled = true;
        assert!(input.state.lock().on_event(CGEventType::LeftMouseUp, &event, None));
        assert!(matches!(
            events_rx.try_recv().unwrap().1,
            Event::MouseUp(crate::actor::drag::MouseButton::Left)
        ));
        assert!(input.state.lock().on_event(CGEventType::LeftMouseUp, &event, None));
        assert!(matches!(
            events_rx.try_recv().unwrap().1,
            Event::MouseUp(crate::actor::drag::MouseButton::Left)
        ));
    }

    #[test]
    fn non_owning_release_does_not_clear_the_captured_button() {
        let (input, _, mut events_rx) = input();
        {
            let mut state = input.state.lock();
            state.mouse_features_enabled = true;
            state.captured_button = Some(crate::actor::drag::MouseButton::Left);
        }
        let right_up = CGEvent::new_mouse_event(
            None,
            CGEventType::RightMouseUp,
            CGPoint::new(20.0, 30.0),
            objc2_core_graphics::CGMouseButton::Right,
        )
        .unwrap();
        assert!(input.state.lock().on_event(CGEventType::RightMouseUp, &right_up, None));
        assert_eq!(
            input.state.lock().captured_button,
            Some(crate::actor::drag::MouseButton::Left)
        );
        assert!(matches!(
            events_rx.try_recv().unwrap().1,
            Event::MouseUp(crate::actor::drag::MouseButton::Right)
        ));
    }

    #[test]
    fn focus_follows_mouse_reset_keeps_a_captured_drag() {
        let (input, _, _) = input();
        let mut state = input.state.lock();
        state.captured_button = Some(crate::actor::drag::MouseButton::Left);
        state.reset(false);
        state.reset(true);
        assert_eq!(
            state.captured_button,
            Some(crate::actor::drag::MouseButton::Left)
        );
    }

    #[test]
    fn drag_motion_uses_rewritten_event_position() {
        for (event_type, button, cg_button) in [
            (
                CGEventType::LeftMouseDragged,
                crate::actor::drag::MouseButton::Left,
                objc2_core_graphics::CGMouseButton::Left,
            ),
            (
                CGEventType::RightMouseDragged,
                crate::actor::drag::MouseButton::Right,
                objc2_core_graphics::CGMouseButton::Right,
            ),
        ] {
            let (input, _, _) = input();
            input.state.lock().captured_button = Some(button);
            let event =
                CGEvent::new_mouse_event(None, event_type, CGPoint::new(99.0, 20.0), cg_button)
                    .unwrap();
            let target = CGPoint::new(6.0, 920.0);
            // Horizontal warping rewrites the event before the drag publisher sees it.
            CGEvent::set_location(Some(&event), target);
            assert!(!input.state.lock().on_event(event_type, &event, None));
            assert_eq!(
                input.state.lock().drag_motion_publisher.take_latest().unwrap().point,
                target
            );
        }
    }

    #[test]
    fn drag_motion_publishes_only_for_captured_or_native_drags() {
        let (input, _, mut events_rx) = input();
        let event = CGEvent::new_mouse_event(
            None,
            CGEventType::LeftMouseDragged,
            CGPoint::new(20.0, 30.0),
            objc2_core_graphics::CGMouseButton::Left,
        )
        .unwrap();
        assert!(input.state.lock().on_event(CGEventType::LeftMouseDragged, &event, None));
        assert!(events_rx.try_recv().is_err());

        input.state.lock().captured_button = Some(crate::actor::drag::MouseButton::Left);
        assert!(!input.state.lock().on_event(CGEventType::LeftMouseDragged, &event, None));
        assert!(matches!(
            events_rx.try_recv().unwrap().1,
            Event::DragMotionPending(_)
        ));
        input.state.lock().drag_motion_publisher.take_latest();

        input.state.lock().captured_button = None;
        input.state.lock().native_motion_active.store(true, Ordering::Release);
        assert!(input.state.lock().on_event(CGEventType::LeftMouseDragged, &event, None));
        assert!(matches!(
            events_rx.try_recv().unwrap().1,
            Event::DragMotionPending(_)
        ));
    }

    #[test]
    fn tap_reenable_hook_defers_recovery_to_the_actor_loop() {
        let (input, _, mut events_rx) = input();
        input.state.lock().pressed_keys.insert(KeyCode::KeyA);
        let (recovery_tx, mut recovery_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut ctx = CallbackCtx {
            state: input.state.clone(),
            recovery_tx,
            tap_generation: AtomicU64::new(7),
        };
        unsafe { event_tap_reenabled((&mut ctx as *mut CallbackCtx).cast()) };
        // Nothing was reconciled inside the callback: no WindowServer flags read, no
        // gesture reset, no key cache clear. The actor loop does that later.
        assert_eq!(recovery_rx.try_recv().unwrap(), Recovery::TapDisabled(7));
        assert!(recovery_rx.try_recv().is_err());
        assert!(input.state.lock().pressed_keys.contains(&KeyCode::KeyA));
        assert!(events_rx.try_recv().is_err());
    }

    #[test]
    fn tap_recovery_discards_cached_keys_and_uses_live_flags() {
        let (input, _, _) = input();
        let mut state = input.state.lock();
        state.pressed_keys.insert(KeyCode::ShiftLeft);
        state.pressed_keys.insert(KeyCode::KeyA);

        let live_flags = CGEventFlags::MaskShift | CGEventFlags::MaskCommand;
        state.reconcile_after_event_tap_reenabled(live_flags);

        assert!(state.pressed_keys.is_empty());
        assert_eq!(state.current_flags, live_flags);
    }

    #[test]
    fn mouse_events_never_query_window_server_on_the_tap_path() {
        let (mut input, _, _) = input();
        let (stack_tx, mut stack_rx) = actor::channel();
        let mut forward_rx = input.forward_rx.take().unwrap();
        let hit = CGPoint::new(20.0, 30.0);
        {
            let mut state = input.state.lock();
            state.stack_line_tx = stack_tx;
            state.event_processing_enabled = true;
            state.stack_line_enabled = true;
            state.cursor_hidden = true;
            state.horizontal_mouse_warp = Some(HorizontalMouseWarp::TopToBottom);
            state.warp_screens = vec![
                CGRect::new(
                    CGPoint::new(0.0, 0.0),
                    objc2_core_foundation::CGSize::new(100.0, 100.0),
                ),
                CGRect::new(
                    CGPoint::new(0.0, 100.0),
                    objc2_core_foundation::CGSize::new(100.0, 100.0),
                ),
            ];
            state.stack_line_hit_rects.store(Arc::new(vec![CGRect::new(
                CGPoint::new(10.0, 20.0),
                objc2_core_foundation::CGSize::new(20.0, 20.0),
            )]));
        }
        let queries = window_server::window_at_point_query_count();
        let moved = CGEvent::new_mouse_event(
            None,
            CGEventType::MouseMoved,
            hit,
            objc2_core_graphics::CGMouseButton::Left,
        )
        .unwrap();
        let down = CGEvent::new_mouse_event(
            None,
            CGEventType::LeftMouseDown,
            hit,
            objc2_core_graphics::CGMouseButton::Left,
        )
        .unwrap();
        let edge = CGEvent::new_mouse_event(
            None,
            CGEventType::MouseMoved,
            CGPoint::new(99.0, 50.0),
            objc2_core_graphics::CGMouseButton::Left,
        )
        .unwrap();
        CGEvent::set_integer_value_field(Some(&edge), CGEventField::MouseEventDeltaX, 4);
        {
            let mut state = input.state.lock();
            assert!(state.on_mouse_moved(&moved, hit));
            // The click is decided from the actor's last occlusion answer, not a query.
            assert!(!state.on_event(CGEventType::LeftMouseDown, &down, None));
            assert_eq!(
                state.maybe_horizontal_mouse_warp(&edge),
                Some(CGPoint::new(6.0, 150.0))
            );
            assert_eq!(CGEvent::location(Some(&edge)), CGPoint::new(6.0, 150.0));
            assert_eq!(state.forward_drops, 0);
        }
        assert_eq!(window_server::window_at_point_query_count(), queries);
        // Cursor show, occlusion and the warp itself were handed to the actor.
        assert_eq!(forward_rx.try_recv().unwrap(), TapEvent::ShowMouse);
        assert_eq!(forward_rx.try_recv().unwrap(), TapEvent::StackLineMove {
            point: hit,
            rect_hit: true
        });
        assert_eq!(forward_rx.try_recv().unwrap(), TapEvent::ShowMouse);
        assert_eq!(
            forward_rx.try_recv().unwrap(),
            TapEvent::Warp(CGPoint::new(6.0, 150.0))
        );
        assert!(forward_rx.try_recv().is_err());
        assert!(
            matches!(stack_rx.try_recv().unwrap().1, stack_line::Event::MouseDown(p) if p == hit)
        );
        // An occluded indicator (actor's answer) lets the click reach the app.
        input.state.lock().stack_line_occluded = true;
        assert!(input.state.lock().on_event(CGEventType::LeftMouseDown, &down, None));
        assert!(stack_rx.try_recv().is_err());
    }

    #[test]
    fn full_forward_channel_passes_the_event_through_and_counts_the_drop() {
        let (input, _, _) = input();
        let mut state = input.state.lock();
        state.event_processing_enabled = true;
        state.cursor_hidden = true;
        let event = CGEvent::new_mouse_event(
            None,
            CGEventType::MouseMoved,
            CGPoint::new(1.0, 1.0),
            objc2_core_graphics::CGMouseButton::Left,
        )
        .unwrap();
        for _ in 0..FORWARD_CAPACITY {
            assert!(state.on_mouse_moved(&event, CGPoint::new(1.0, 1.0)));
        }
        assert_eq!(state.forward_drops, 0);
        assert!(state.on_mouse_moved(&event, CGPoint::new(1.0, 1.0)));
        assert!(state.on_event(CGEventType::LeftMouseDown, &event, None));
        assert_eq!(state.forward_drops, 2);
    }

    #[test]
    fn callback_passes_events_through_while_the_state_lock_is_busy() {
        let (input, mut wm_rx, _) = input();
        input.hotkeys_active.set(true);
        input.state.lock().rebuild_binding_maps(&input.binding_mode_specs.borrow());
        input.state.lock().hotkeys[0].insert(Hotkey::new(Modifiers::empty(), KeyCode::KeyA), vec![
            WmCommand::Wm(wm_controller::WmCmd::ReloadConfig),
        ]);
        let (recovery_tx, _) = tokio::sync::mpsc::unbounded_channel();
        let state = input.state.clone();
        let misses = CALLBACK_LOCK_MISSES.load(Ordering::Relaxed);
        let callback = move |ctx: &CallbackCtx| {
            let event = CGEvent::new_keyboard_event(None, 0, true).unwrap();
            CGEvent::set_flags(Some(&event), CGEventFlags::empty());
            let event_ptr = core::ptr::NonNull::from(&*event);
            let result = unsafe {
                input_callback(
                    core::ptr::null_mut(),
                    CGEventType::KeyDown,
                    event_ptr,
                    (ctx as *const CallbackCtx).cast_mut().cast(),
                )
            };
            result == event_ptr.as_ptr()
        };
        let ctx = CallbackCtx {
            state,
            recovery_tx,
            tap_generation: AtomicU64::new(0),
        };
        let guard = input.state.lock();
        // The bound hotkey passes through, within CALLBACK_LOCK_WAIT, instead of blocking.
        let passed = std::thread::scope(|s| s.spawn(|| callback(&ctx)).join().unwrap());
        assert!(passed);
        assert_eq!(CALLBACK_LOCK_MISSES.load(Ordering::Relaxed), misses + 1);
        assert!(wm_rx.try_recv().is_err());
        drop(guard);
        assert!(!callback(&ctx));
        assert!(wm_rx.try_recv().is_ok());
    }
}
