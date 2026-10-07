//! Workspace HUD actor (main thread): shows the card the reactor asks for.
//!
//! The reactor sends one [`Event::Show`] the moment it decides a workspace
//! switch, before any focus or raise work, and the display list whenever it
//! changes. The overlays (one per display) are built when the HUD is enabled,
//! restyled or the displays change, never on a show; see
//! [`crate::ui::workspace_hud`] for what a show costs. One manual timer drives
//! the shown time and the fade ([`HudPresenter`]).

use std::time::{Duration, Instant};

use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_foundation::NSString;
use tracing::{debug, warn};

use crate::actor;
use crate::common::collections::HashMap;
use crate::common::config::{Config, WorkspaceHudSettings};
use crate::model::workspace_hud::{
    HudDisplay, HudDisplayChoice, HudPresenter, HudStyle, MAX_HUD_DISPLAYS, TimerStep,
    choose_displays, mask_indices,
};
use crate::sys::timer::Timer;
use crate::ui::workspace_hud::{HudFont, HudWindow};

/// A workspace switch the reactor decided.
#[derive(Debug, Clone)]
pub struct ShowRequest {
    /// The rendered `text` template.
    pub text: String,
    /// The display that shows the workspace.
    pub target: String,
    /// The display that had focus when the switch was asked for.
    pub focused: Option<String>,
    /// When the reactor decided the switch (for the timing log).
    pub decided_at: Instant,
}

#[derive(Debug)]
pub enum Event {
    Show(ShowRequest),
    /// The displays rift knows, with their visible frames.
    DisplaysChanged(Vec<HudDisplay>),
    ConfigUpdated(Config),
}

pub type Sender = actor::Sender<Event>;
pub type Receiver = actor::Receiver<Event>;

/// Texts the HUD has shown, measured once; bounded so dynamic names cannot
/// grow it forever.
const MAX_CACHED_TEXTS: usize = 128;

struct CachedText {
    string: Retained<NSString>,
    width: f64,
}

/// Everything built for one style and display set.
struct Overlays {
    style: HudStyle,
    font: HudFont,
    /// Parallel to `WorkspaceHud::displays`; `None` where building failed.
    windows: Vec<Option<HudWindow>>,
    texts: HashMap<String, CachedText>,
}

impl Overlays {
    fn build(style: HudStyle, displays: &[HudDisplay]) -> Self {
        let font = HudFont::resolve(&style);
        let windows = displays
            .iter()
            .take(MAX_HUD_DISPLAYS)
            .map(|screen| match HudWindow::new(screen, &style, &font) {
                Ok(window) => Some(window),
                Err(error) => {
                    warn!(?error, uuid = %screen.uuid, "failed to create the workspace HUD");
                    None
                }
            })
            .collect();
        Self {
            style,
            font,
            windows,
            texts: HashMap::default(),
        }
    }

    fn available(&self) -> u64 {
        self.windows
            .iter()
            .enumerate()
            .filter(|(_, window)| window.is_some())
            .fold(0, |mask, (index, _)| mask | (1 << index))
    }

    /// The cached string and width of `text`, measuring it the first time.
    fn text(&mut self, text: &str) -> (Retained<NSString>, f64) {
        if let Some(cached) = self.texts.get(text) {
            return (cached.string.clone(), cached.width);
        }
        if self.texts.len() >= MAX_CACHED_TEXTS {
            self.texts.clear();
        }
        let string = NSString::from_str(text);
        // A fixed-size card never needs the width.
        let width = if self.style.is_auto_sized() {
            self.font.measure(&string)
        } else {
            0.0
        };
        self.texts
            .insert(text.to_string(), CachedText { string: string.clone(), width });
        (string, width)
    }
}

pub struct WorkspaceHud {
    settings: WorkspaceHudSettings,
    rx: Receiver,
    #[allow(dead_code)]
    mtm: MainThreadMarker,
    displays: Vec<HudDisplay>,
    overlays: Option<Overlays>,
    presenter: HudPresenter,
}

impl WorkspaceHud {
    pub fn new(config: &Config, rx: Receiver, mtm: MainThreadMarker) -> Self {
        Self {
            settings: config.settings.ui.workspace_hud.clone(),
            rx,
            mtm,
            displays: Vec::new(),
            overlays: None,
            presenter: HudPresenter::default(),
        }
    }

    pub async fn run(mut self) {
        // One timer for the whole life of the actor, re-armed per step.
        let mut timer = Timer::manual();
        loop {
            tokio::select! {
                message = self.rx.recv() => {
                    let Some((span, event)) = message else { break };
                    let _guard = span.enter();
                    self.handle_event(event, &timer);
                }
                Some(()) = timer.next() => self.timer_fired(&timer),
            }
        }
    }

    fn handle_event(&mut self, event: Event, timer: &Timer) {
        match event {
            Event::Show(request) => self.show(request, timer),
            Event::DisplaysChanged(displays) => {
                if displays != self.displays {
                    self.displays = displays;
                    self.rebuild();
                }
            }
            Event::ConfigUpdated(config) => {
                let settings = config.settings.ui.workspace_hud;
                if settings == self.settings {
                    return;
                }
                let restyle = settings.enabled != self.settings.enabled
                    || self
                        .overlays
                        .as_ref()
                        .is_none_or(|overlays| overlays.style != settings.resolve());
                self.settings = settings;
                if restyle {
                    self.rebuild();
                }
            }
        }
    }

    /// Drop the overlays (taking any on screen off it) and, when enabled,
    /// build them for the current style and displays.
    fn rebuild(&mut self) {
        self.presenter.reset();
        self.overlays = None;
        if self.settings.enabled && !self.displays.is_empty() {
            self.overlays = Some(Overlays::build(self.settings.resolve(), &self.displays));
        }
    }

    fn show(&mut self, request: ShowRequest, timer: &Timer) {
        let Some(overlays) = &mut self.overlays else { return };
        let cursor = (self.settings.display == HudDisplayChoice::Cursor)
            .then(|| crate::sys::window_server::current_cursor_location().ok())
            .flatten();
        let chosen = choose_displays(
            self.settings.display,
            &self.displays,
            &request.target,
            request.focused.as_deref(),
            cursor,
        ) & overlays.available();
        let plan = self.presenter.show(chosen);
        for index in mask_indices(plan.order_out) {
            if let Some(window) = &mut overlays.windows[index] {
                window.order_out();
            }
        }
        if plan.update == 0 {
            return;
        }
        let (text, width) = overlays.text(&request.text);
        for index in mask_indices(plan.update) {
            let Some(window) = &mut overlays.windows[index] else {
                continue;
            };
            if let Err(error) = window.present(
                &self.displays[index],
                &overlays.style,
                &overlays.font,
                &text,
                width,
            ) {
                warn!(
                    ?error,
                    "failed to show the workspace HUD; rebuilding it on the next change"
                );
                overlays.windows[index] = None;
            }
        }
        timer.set_next_fire(Duration::from_secs_f64(
            self.settings.duration_ms.max(0.0) / 1000.0,
        ));
        debug!(
            text = %request.text,
            decision_to_commit_us = request.decided_at.elapsed().as_micros() as u64,
            "workspace HUD shown"
        );
    }

    fn timer_fired(&mut self, timer: &Timer) {
        let Some(overlays) = &mut self.overlays else {
            self.presenter.reset();
            return;
        };
        match self.presenter.timer_fired() {
            TimerStep::FadeOut(mask) => {
                for index in mask_indices(mask) {
                    if let Some(window) = &overlays.windows[index] {
                        window.fade_out(overlays.style.fade_ms);
                    }
                }
                timer.set_next_fire(Duration::from_secs_f64(
                    overlays.style.fade_ms.max(0.0) / 1000.0,
                ));
            }
            TimerStep::OrderOut(mask) => {
                for index in mask_indices(mask) {
                    if let Some(window) = &mut overlays.windows[index] {
                        window.order_out();
                    }
                }
            }
            TimerStep::Idle => {}
        }
    }
}
