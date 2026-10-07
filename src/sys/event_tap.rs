use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use objc2_core_foundation::{
    CFMachPort, CFRetained, CFRunLoop, CFRunLoopMode, CFRunLoopSource, kCFRunLoopCommonModes,
};
use objc2_core_graphics::{
    CGEvent, CGEventMask, CGEventTapLocation as CGTapLoc, CGEventTapOptions as CGTapOpt,
    CGEventTapPlacement as CGTapPlace, CGEventTapProxy, CGEventType,
};
use parking_lot::Mutex;
use tracing::warn;

use super::run_loop::WakeupHandle;

/// A thread whose run loop services event taps and nothing else.
///
/// WindowServer waits for an active tap's callback before delivering each
/// masked key and click, so the thread that runs the callback must never be
/// busy with a synchronous WindowServer call. Taps are created and torn down
/// from other threads; their callback contexts are freed here, serialized with
/// the callbacks, so a callback in flight never sees a freed context.
pub struct TapThread {
    run_loop: CFRetained<CFRunLoop>,
    retired: Arc<Mutex<Vec<Retired>>>,
    wake: WakeupHandle,
}

struct Retired(*mut c_void, unsafe fn(*mut c_void));

// SAFETY: a retired context is only ever dropped on the tap thread, through the
// dropper it was created with. CFRunLoop is thread-safe; this handle only adds
// and removes sources and wakes the loop.
unsafe impl Send for Retired {}
unsafe impl Send for TapThread {}
unsafe impl Sync for TapThread {}

struct Handoff(CFRetained<CFRunLoop>, WakeupHandle);
unsafe impl Send for Handoff {}

impl TapThread {
    pub fn spawn() -> Option<Arc<Self>> {
        let retired: Arc<Mutex<Vec<Retired>>> = Arc::default();
        let graveyard = retired.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("event-tap".into())
            .spawn(move || {
                set_user_interactive_qos();
                // Also keeps the loop alive while no tap is installed.
                let wake = WakeupHandle::for_current_thread(0, move || {
                    for Retired(ptr, dropper) in std::mem::take(&mut *graveyard.lock()) {
                        unsafe { dropper(ptr) };
                    }
                });
                let run_loop = CFRunLoop::current().expect("event tap thread has a run loop");
                let _ = tx.send(Handoff(run_loop, wake));
                loop {
                    CFRunLoop::run();
                }
            })
            .ok()?;
        let Handoff(run_loop, wake) = rx.recv().ok()?;
        Some(Arc::new(Self { run_loop, retired, wake }))
    }

    /// Frees `ptr` with `dropper` on the tap thread, after any callback in flight.
    pub(crate) fn retire(&self, ptr: *mut c_void, dropper: unsafe fn(*mut c_void)) {
        self.retired.lock().push(Retired(ptr, dropper));
        self.wake.wake();
    }
}

fn set_user_interactive_qos() {
    const QOS_CLASS_USER_INTERACTIVE: u32 = 0x21;
    unsafe extern "C" {
        fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
    }
    if unsafe { pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0) } != 0 {
        warn!("Could not raise the event tap thread to user-interactive QoS");
    }
}

pub type TapCallback = Option<
    unsafe extern "C-unwind" fn(
        CGEventTapProxy,
        CGEventType,
        core::ptr::NonNull<CGEvent>,
        *mut c_void,
    ) -> *mut CGEvent,
>;

/// Called inside the tap callback right after a disabled tap was re-enabled,
/// and again when a breaker backoff ends (input changed unseen meanwhile). It
/// must not block: no WindowServer, AX or lock that the reactor can hold. Hand
/// the work to another thread and return.
pub type TapReenabledCallback = Option<unsafe extern "C-unwind" fn(*mut c_void)>;
pub type TapInvalidatedCallback = Option<unsafe extern "C-unwind" fn(*mut c_void)>;

/// Default for `settings.event_tap_timeout_limit`: WindowServer timeouts within
/// [`TIMEOUT_WINDOW`] before the tap passes everything through for a backoff.
pub const DEFAULT_TIMEOUT_LIMIT: u32 = 3;
pub const TIMEOUT_WINDOW: Duration = Duration::from_secs(60);
pub const INITIAL_BACKOFF: Duration = Duration::from_secs(5);
pub const MAX_BACKOFF: Duration = Duration::from_secs(60);

static TIMEOUT_LIMIT: AtomicU32 = AtomicU32::new(DEFAULT_TIMEOUT_LIMIT);

/// Applies to taps created afterwards; 0 never trips the breaker.
pub fn set_timeout_limit(limit: u32) { TIMEOUT_LIMIT.store(limit, Ordering::Relaxed); }

/// Circuit breaker for `kCGEventTapDisabledByTimeout` / `ByUserInput`.
///
/// WindowServer disables an active tap whose callback does not answer in time
/// and holds every masked event behind it. Re-enabling at once restarts the
/// stall when the cause persists, so after `limit` timeouts within `window` the
/// tap stops filtering (hotkeys inactive, nothing consumed) for a backoff that
/// doubles up to `max_backoff`, then tries again. The user keeps typing no
/// matter what. Pure: callers pass the clock.
#[derive(Debug)]
pub struct Breaker {
    limit: u32,
    window: Duration,
    initial_backoff: Duration,
    max_backoff: Duration,
    recent: VecDeque<Instant>,
    passthrough_until: Option<Instant>,
    next_backoff: Duration,
    rearmed_at: Option<Instant>,
    trips: u32,
}

impl Breaker {
    pub fn new(limit: u32, window: Duration, backoff: Duration, max_backoff: Duration) -> Self {
        Self {
            limit,
            window,
            initial_backoff: backoff,
            max_backoff,
            recent: VecDeque::new(),
            passthrough_until: None,
            next_backoff: backoff,
            rearmed_at: None,
            trips: 0,
        }
    }

    /// Counts one disable. Returns the backoff when this one trips the breaker.
    pub fn record_disabled(&mut self, now: Instant) -> Option<Duration> {
        // A full quiet window after the last re-arm forgives earlier trips.
        if self.rearmed_at.is_some_and(|t| now.duration_since(t) >= self.window) {
            self.next_backoff = self.initial_backoff;
            self.rearmed_at = None;
        }
        self.recent.push_back(now);
        while self.recent.front().is_some_and(|&t| now.duration_since(t) >= self.window) {
            self.recent.pop_front();
        }
        if self.passthrough_until.is_some()
            || self.limit == 0
            || self.recent.len() < self.limit as usize
        {
            return None;
        }
        let backoff = self.next_backoff;
        self.passthrough_until = Some(now + backoff);
        self.next_backoff = (backoff * 2).min(self.max_backoff);
        self.trips += 1;
        self.recent.clear();
        Some(backoff)
    }

    /// True while input must pass through untouched. Re-arms once the backoff
    /// elapsed; the next `limit` timeouts start a fresh count.
    pub fn passing_through(&mut self, now: Instant) -> bool {
        match self.passthrough_until {
            Some(until) if now < until => true,
            Some(_) => {
                self.passthrough_until = None;
                self.rearmed_at = Some(now);
                self.recent.clear();
                false
            }
            None => false,
        }
    }

    pub fn recent_timeouts(&self) -> usize { self.recent.len() }

    pub fn trips(&self) -> u32 { self.trips }
}

struct TrampolineCtx {
    callback: TapCallback,
    original_user_info: *mut c_void,
    original_drop: Option<unsafe fn(*mut c_void)>,
    reenabled_callback: TapReenabledCallback,
    invalidated_callback: TapInvalidatedCallback,
    /// Own retain: the owner may release its `EventTap` on another thread while
    /// a callback here still re-enables the port.
    port: Option<CFRetained<CFMachPort>>,
    breaker: RefCell<Breaker>,
    passthrough: Cell<bool>,
}

extern "C-unwind" fn port_invalidated(_port: *mut CFMachPort, user_info: *mut c_void) {
    if user_info.is_null() {
        return;
    }

    let ctx = unsafe { &*(user_info as *const TrampolineCtx) };
    warn!("Event tap Mach port was invalidated; scheduling tap recreation");
    if let Some(callback) = ctx.invalidated_callback {
        unsafe { callback(ctx.original_user_info) };
    }
}

extern "C-unwind" fn trampoline_callback(
    proxy: CGEventTapProxy,
    etype: CGEventType,
    event_ref: core::ptr::NonNull<CGEvent>,
    user_info: *mut c_void,
) -> *mut CGEvent {
    if user_info.is_null() {
        return event_ref.as_ptr();
    }

    let ctx = unsafe { &*(user_info as *const TrampolineCtx) };

    // kCGEventTapDisabledByTimeout (-2) & kCGEventTapDisabledByUserInput (-1)
    let ety = etype.0 as i32;
    if ety == -1 || ety == -2 {
        let reason = if ety == -2 { "timeout" } else { "user input" };
        // The only WindowServer call allowed here. Whether it took effect is
        // checked by the owner on its own thread (see TapReenabledCallback).
        if let Some(port) = &ctx.port {
            CGEvent::tap_enable(port, true);
        }
        let mut breaker = ctx.breaker.borrow_mut();
        match breaker.record_disabled(Instant::now()) {
            Some(backoff) => {
                ctx.passthrough.set(true);
                warn!(
                    reason,
                    trips = breaker.trips(),
                    backoff_secs = backoff.as_secs(),
                    "Event tap was disabled too often; passing all input through until the \
                     backoff elapses"
                );
            }
            None => warn!(
                reason,
                recent = breaker.recent_timeouts(),
                "Event tap was disabled; re-enabling it"
            ),
        }
        drop(breaker);
        if let Some(callback) = ctx.reenabled_callback {
            unsafe { callback(ctx.original_user_info) };
        }
        return event_ref.as_ptr();
    }

    if ctx.passthrough.get() {
        if ctx.breaker.borrow_mut().passing_through(Instant::now()) {
            return event_ref.as_ptr();
        }
        ctx.passthrough.set(false);
        warn!("Event tap backoff elapsed; filtering input again");
        // Keys, modifiers and buttons changed unseen during the backoff; let the
        // owner reconcile its input state on its own thread before it trusts it.
        if let Some(callback) = ctx.reenabled_callback {
            unsafe { callback(ctx.original_user_info) };
        }
    }

    if let Some(orig_cb) = ctx.callback {
        return unsafe { orig_cb(proxy, etype, event_ref, ctx.original_user_info) };
    }

    event_ref.as_ptr()
}

unsafe fn trampoline_drop(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }

    let ctx: Box<TrampolineCtx> = unsafe { Box::from_raw(ptr as *mut TrampolineCtx) };
    if let Some(dropper) = ctx.original_drop {
        if !ctx.original_user_info.is_null() {
            unsafe { dropper(ctx.original_user_info) };
        }
    }
}

pub struct EventTap {
    port: CFRetained<CFMachPort>,
    source: CFRetained<CFRunLoopSource>,
    run_loop: Option<CFRetained<CFRunLoop>>,
    thread: Option<Arc<TapThread>>,
    user_info: *mut c_void,
    drop_ctx: Option<unsafe fn(*mut c_void)>,
}

impl EventTap {
    /// Creates a tap serviced by `thread`, or by the current run loop when
    /// `thread` is `None` (Rift input uses active HID).
    /// On failure the caller retains ownership of `user_info`.
    pub unsafe fn new(
        location: CGTapLoc,
        options: CGTapOpt,
        mask: CGEventMask,
        callback: TapCallback,
        user_info: *mut c_void,
        drop_ctx: Option<unsafe fn(*mut c_void)>,
        reenabled_callback: TapReenabledCallback,
        invalidated_callback: TapInvalidatedCallback,
        thread: Option<&Arc<TapThread>>,
    ) -> Option<Self> {
        let tramp = Box::new(TrampolineCtx {
            callback,
            original_user_info: user_info,
            original_drop: drop_ctx,
            reenabled_callback,
            invalidated_callback,
            port: None,
            breaker: RefCell::new(Breaker::new(
                TIMEOUT_LIMIT.load(Ordering::Relaxed),
                TIMEOUT_WINDOW,
                INITIAL_BACKOFF,
                MAX_BACKOFF,
            )),
            passthrough: Cell::new(false),
        });
        let tramp_ptr = Box::into_raw(tramp) as *mut c_void;

        let port = unsafe {
            CGEvent::tap_create(
                location,
                CGTapPlace::HeadInsertEventTap,
                options,
                mask,
                Some(trampoline_callback),
                tramp_ptr,
            )
        };
        let Some(port) = port else {
            unsafe {
                drop(Box::from_raw(tramp_ptr as *mut TrampolineCtx));
            }
            return None;
        };
        let Some(source) = CFMachPort::new_run_loop_source(None, Some(&port), 0) else {
            port.invalidate();
            unsafe {
                drop(Box::from_raw(tramp_ptr as *mut TrampolineCtx));
            }
            return None;
        };
        let run_loop = match thread {
            Some(thread) => Some(thread.run_loop.clone()),
            None => CFRunLoop::current(),
        };
        unsafe {
            // Set before the source is scheduled: the first callback may be a
            // disable notice that has to re-enable the port.
            let tramp_ctx = &mut *(tramp_ptr as *mut TrampolineCtx);
            tramp_ctx.port = Some(port.clone());
            port.set_invalidation_call_back(Some(port_invalidated));
        }
        if let Some(rl) = &run_loop {
            let mode: &CFRunLoopMode = unsafe {
                kCFRunLoopCommonModes.expect("kCFRunLoopCommonModes should be available on macOS")
            };
            rl.add_source(Some(&source), Some(mode));
        }
        CGEvent::tap_enable(&port, true);

        Some(Self {
            port,
            source,
            run_loop,
            thread: thread.cloned(),
            user_info: tramp_ptr,
            drop_ctx: Some(trampoline_drop),
        })
    }

    /// Synchronous WindowServer query; never call it from the tap callback.
    pub fn is_enabled(&self) -> bool { CGEvent::tap_is_enabled(&self.port) }
}

impl Drop for EventTap {
    fn drop(&mut self) {
        if self.port.is_valid() {
            // Intentional teardown/replacement must not be mistaken for an
            // unexpected Mach-port failure by the event-driven recovery path.
            unsafe { self.port.set_invalidation_call_back(None) };
            CGEvent::tap_enable(&self.port, false);
        }
        if let Some(rl) = &self.run_loop {
            rl.remove_source(Some(&self.source), unsafe { kCFRunLoopCommonModes });
        }
        self.port.invalidate();
        if let Some(dropper) = self.drop_ctx {
            match &self.thread {
                // A callback may still be running there; let that thread free the context.
                Some(thread) => thread.retire(self.user_info, dropper),
                None => unsafe { dropper(self.user_info) },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    #[derive(Default)]
    struct Calls {
        filtered: AtomicUsize,
        reconciles: AtomicUsize,
    }

    unsafe extern "C-unwind" fn count_filtered(
        _: CGEventTapProxy,
        _: CGEventType,
        event: core::ptr::NonNull<CGEvent>,
        user_info: *mut c_void,
    ) -> *mut CGEvent {
        unsafe { &*(user_info as *const Calls) }
            .filtered
            .fetch_add(1, Ordering::Relaxed);
        event.as_ptr()
    }

    unsafe extern "C-unwind" fn count_reconcile(user_info: *mut c_void) {
        unsafe { &*(user_info as *const Calls) }
            .reconciles
            .fetch_add(1, Ordering::Relaxed);
    }

    #[test]
    fn backoff_passes_input_through_then_asks_the_owner_to_reconcile() {
        let calls = Calls::default();
        let ctx = TrampolineCtx {
            callback: Some(count_filtered),
            original_user_info: (&calls as *const Calls).cast_mut().cast(),
            original_drop: None,
            reenabled_callback: Some(count_reconcile),
            invalidated_callback: None,
            port: None,
            breaker: RefCell::new(Breaker::new(
                1,
                Duration::from_secs(60),
                Duration::from_millis(30),
                Duration::from_millis(30),
            )),
            passthrough: Cell::new(false),
        };
        // Neither path below dereferences the event.
        let event = core::ptr::NonNull::<CGEvent>::dangling();
        let call = |ty| {
            trampoline_callback(
                core::ptr::null_mut(),
                ty,
                event,
                (&ctx as *const TrampolineCtx).cast_mut().cast(),
            )
        };
        let count = |c: &AtomicUsize| c.load(Ordering::Relaxed);

        assert_eq!(call(CGEventType::TapDisabledByTimeout), event.as_ptr());
        assert_eq!(count(&calls.reconciles), 1, "the disable itself is reconciled");
        assert_eq!(call(CGEventType::KeyDown), event.as_ptr());
        assert_eq!(count(&calls.filtered), 0, "backoff: nothing reaches the filter");
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(call(CGEventType::KeyDown), event.as_ptr());
        assert_eq!(count(&calls.filtered), 1);
        assert_eq!(count(&calls.reconciles), 2, "backoff end is reconciled once");
        call(CGEventType::KeyDown);
        assert_eq!(count(&calls.reconciles), 2);
    }

    fn breaker() -> Breaker {
        Breaker::new(
            3,
            Duration::from_secs(60),
            Duration::from_secs(5),
            Duration::from_secs(60),
        )
    }

    #[test]
    fn timeouts_inside_the_window_trip_the_breaker_and_backoff_doubles() {
        let mut b = breaker();
        let t0 = Instant::now();
        assert_eq!(b.record_disabled(t0), None);
        assert_eq!(b.record_disabled(t0 + Duration::from_secs(10)), None);
        assert!(!b.passing_through(t0 + Duration::from_secs(10)));
        assert_eq!(b.recent_timeouts(), 2);
        assert_eq!(
            b.record_disabled(t0 + Duration::from_secs(20)),
            Some(Duration::from_secs(5))
        );
        assert_eq!(b.trips(), 1);
        assert!(b.passing_through(t0 + Duration::from_secs(20)));
        assert!(b.passing_through(t0 + Duration::from_secs(24)));
        // Timeouts during the backoff neither extend it nor trip again.
        assert_eq!(b.record_disabled(t0 + Duration::from_secs(22)), None);
        assert!(!b.passing_through(t0 + Duration::from_secs(25)));
        // Re-armed: a fresh count of `limit` timeouts is needed, then the backoff doubles.
        let t1 = t0 + Duration::from_secs(26);
        assert_eq!(b.record_disabled(t1), None);
        assert_eq!(b.record_disabled(t1 + Duration::from_secs(1)), None);
        assert_eq!(
            b.record_disabled(t1 + Duration::from_secs(2)),
            Some(Duration::from_secs(10))
        );
        assert!(b.passing_through(t1 + Duration::from_secs(11)));
        assert!(!b.passing_through(t1 + Duration::from_secs(12)));
        for _ in 0..2 {
            assert_eq!(b.record_disabled(t1 + Duration::from_secs(13)), None);
        }
        assert_eq!(
            b.record_disabled(t1 + Duration::from_secs(13)),
            Some(Duration::from_secs(20))
        );
    }

    #[test]
    fn backoff_is_capped_and_resets_after_a_quiet_window() {
        let mut b = Breaker::new(
            1,
            Duration::from_secs(60),
            Duration::from_secs(40),
            Duration::from_secs(60),
        );
        let t0 = Instant::now();
        assert_eq!(b.record_disabled(t0), Some(Duration::from_secs(40)));
        assert!(!b.passing_through(t0 + Duration::from_secs(40)));
        assert_eq!(
            b.record_disabled(t0 + Duration::from_secs(41)),
            Some(Duration::from_secs(60))
        );
        assert!(!b.passing_through(t0 + Duration::from_secs(101)));
        assert_eq!(
            b.record_disabled(t0 + Duration::from_secs(102)),
            Some(Duration::from_secs(60))
        );
        assert!(!b.passing_through(t0 + Duration::from_secs(162)));
        // A whole window without a timeout after re-arming forgives the escalation.
        assert_eq!(
            b.record_disabled(t0 + Duration::from_secs(222)),
            Some(Duration::from_secs(40))
        );
    }

    #[test]
    fn old_timeouts_fall_out_of_the_window_and_zero_limit_never_trips() {
        let mut b = breaker();
        let t0 = Instant::now();
        assert_eq!(b.record_disabled(t0), None);
        assert_eq!(b.record_disabled(t0 + Duration::from_secs(30)), None);
        assert_eq!(b.record_disabled(t0 + Duration::from_secs(61)), None);
        assert_eq!(b.recent_timeouts(), 2);
        assert!(!b.passing_through(t0 + Duration::from_secs(61)));

        let mut off = Breaker::new(
            0,
            Duration::from_secs(60),
            Duration::from_secs(5),
            Duration::from_secs(60),
        );
        for i in 0..10 {
            assert_eq!(off.record_disabled(t0 + Duration::from_secs(i)), None);
        }
        assert!(!off.passing_through(t0 + Duration::from_secs(10)));
    }
}
