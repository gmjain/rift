//! Floating windows above tiled ones (`[settings] floating_windows_on_top`, i3 semantics).
//!
//! macOS puts the window that gains focus on top, so a floating window or dialog disappears
//! behind the tiled window the user focuses next. With the setting on, every focus change to a
//! tiled window is followed by one "float pass": the visible floating windows of that window's
//! workspace are AX-raised above it, least recently focused first, without activating their
//! apps (see `Request::Restack`). The pass rides on rift's own raise sequence when the focus
//! change is rift's doing, and is a sequence of its own when macOS reports the change (a click).
//!
//! This is the bookkeeping behind it: the focus order of windows (bottom-to-top order of a
//! pass), the pass computed for the focus change in hand, and the guards that keep passes
//! from feeding back on themselves:
//!
//! - one pass per focus change: the events a pass echoes (the target app's activation, the
//!   WindowServer focus report, a snapshot re-sending the focused window) all name the same
//!   target and floats within a second and are dropped;
//! - a rate limit: more than [`MAX_PASSES_PER_SECOND`] means something feeds focus changes
//!   back, so passes stop for [`BACKOFF`];
//! - apps that activate themselves when one of their floats is raised (focus then jumps to
//!   the float) are detected and left out of passes for [`SELF_ACTIVATION_EXCLUSION`].

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use tracing::{debug, warn};

use crate::actor::app::{WindowId, pid_t};
use crate::common::collections::HashMap;
use crate::sys::screen::SpaceId;

/// A float pass: `windows` to raise above `target`, bottom to top.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FloatPass {
    pub target: WindowId,
    pub windows: Vec<WindowId>,
}

/// Windows remembered in focus order.
const FOCUS_ORDER_LIMIT: usize = 512;
/// A pass equal to one sent this recently is an echo of it.
const ECHO_WINDOW: Duration = Duration::from_secs(1);
/// Passes allowed per second before backing off.
pub(crate) const MAX_PASSES_PER_SECOND: usize = 8;
/// How long passes stop once the rate limit trips.
const BACKOFF: Duration = Duration::from_secs(1);
/// A quiet app activation this soon after a pass that raised one of its floats, with no raise
/// of that app requested meanwhile, is the app activating itself on AXRaise.
const SELF_ACTIVATION_WINDOW: Duration = Duration::from_secs(2);
/// How long a self-activating app's floats are left out of passes. Short enough that a user
/// activation misattributed by the app thread's one-second "by us" heuristic costs little;
/// a true self-activator then bounces focus at most once per this period.
const SELF_ACTIVATION_EXCLUSION: Duration = Duration::from_secs(30);

#[derive(Default)]
pub(crate) struct FloatStack {
    /// Windows in focus order, least recently focused first.
    focus_order: VecDeque<WindowId>,
    /// The focus change being handled (space, focused window), until its pass is attached
    /// to that change's raise request or sent on its own. The pass itself is computed when
    /// it is sent, after the event's other effects.
    pending: Option<(SpaceId, WindowId)>,
    /// The last pass sent and when.
    last_sent: Option<(FloatPass, Instant)>,
    /// When the passes of the last second were sent.
    recent: VecDeque<Instant>,
    backoff_until: Option<Instant>,
    /// Apps rift asked to raise (activate) recently: their activations are not self-activations.
    recent_raises: VecDeque<(pid_t, Instant)>,
    /// Apps that activated themselves on a restack, and until when they are excluded.
    self_activating: HashMap<pid_t, Instant>,
}

impl FloatStack {
    /// `window` gained focus (rift raised it, or macOS reported it).
    pub(crate) fn note_focus(&mut self, window: WindowId) {
        self.focus_order.retain(|w| *w != window);
        self.focus_order.push_back(window);
        while self.focus_order.len() > FOCUS_ORDER_LIMIT {
            self.focus_order.pop_front();
        }
        // Focus moved elsewhere: coming back to the target needs a pass of its own.
        if self.last_sent.as_ref().is_some_and(|(pass, _)| pass.target != window) {
            self.last_sent = None;
        }
    }

    /// `windows` in focus order, least recently focused first; windows never focused come
    /// first, in id order.
    pub(crate) fn ordered(&self, mut windows: Vec<WindowId>) -> Vec<WindowId> {
        let position = |window: WindowId| {
            self.focus_order.iter().position(|w| *w == window).map_or(0, |p| p + 1)
        };
        windows.sort_by_key(|window| (position(*window), *window));
        windows.dedup();
        windows
    }

    pub(crate) fn queue(&mut self, space: SpaceId, target: WindowId) {
        self.pending = Some((space, target));
    }

    pub(crate) fn take_pending(&mut self) -> Option<(SpaceId, WindowId)> { self.pending.take() }

    /// The space of the pending focus change when it is to `target`.
    pub(crate) fn take_pending_for(&mut self, target: WindowId) -> Option<SpaceId> {
        match self.pending {
            Some((space, pending)) if pending == target => {
                self.pending = None;
                Some(space)
            }
            _ => None,
        }
    }

    /// Whether `pass` may be sent now; records it when so.
    pub(crate) fn admit(&mut self, pass: &FloatPass, now: Instant) -> bool {
        if let Some(until) = self.backoff_until {
            if now < until {
                return false;
            }
            self.backoff_until = None;
        }
        if let Some((last, at)) = &self.last_sent
            && last == pass
            && now.duration_since(*at) < ECHO_WINDOW
        {
            return false;
        }
        self.recent.retain(|at| now.duration_since(*at) < Duration::from_secs(1));
        if self.recent.len() >= MAX_PASSES_PER_SECOND {
            warn!(
                ?pass,
                "{MAX_PASSES_PER_SECOND} float passes within a second; backing off for {BACKOFF:?}"
            );
            self.recent.clear();
            self.backoff_until = Some(now + BACKOFF);
            return false;
        }
        self.recent.push_back(now);
        self.last_sent = Some((pass.clone(), now));
        true
    }

    /// Rift asked to raise (and so activate) windows of these apps.
    pub(crate) fn note_raise(&mut self, pids: impl IntoIterator<Item = pid_t>, now: Instant) {
        self.recent_raises
            .retain(|(_, at)| now.duration_since(*at) < SELF_ACTIVATION_WINDOW);
        for pid in pids {
            self.recent_raises.push_back((pid, now));
        }
    }

    /// `pid` activated and its app thread attributed that to rift (quiet). Returns `true`
    /// if the cause is the last pass raising one of its floats, i.e. no raise of `pid` was
    /// requested; the app then stays out of passes for a while.
    pub(crate) fn note_app_activated(&mut self, pid: pid_t, now: Instant) -> bool {
        let Some((pass, at)) = &self.last_sent else {
            return false;
        };
        if now.duration_since(*at) >= SELF_ACTIVATION_WINDOW
            || pass.target.pid == pid
            || !pass.windows.iter().any(|window| window.pid == pid)
        {
            return false;
        }
        if self
            .recent_raises
            .iter()
            .any(|(raised, at)| *raised == pid && now.duration_since(*at) < SELF_ACTIVATION_WINDOW)
        {
            return false;
        }
        warn!(
            pid,
            "app activated itself when its floating window was raised; leaving its floating \
             windows out of float passes for {SELF_ACTIVATION_EXCLUSION:?}"
        );
        self.self_activating.insert(pid, now + SELF_ACTIVATION_EXCLUSION);
        true
    }

    pub(crate) fn is_excluded(&self, pid: pid_t, now: Instant) -> bool {
        self.self_activating.get(&pid).is_some_and(|until| now < *until)
    }

    pub(crate) fn forget_app(&mut self, pid: pid_t) {
        if self.self_activating.remove(&pid).is_some() {
            debug!(pid, "self-activating app exited");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pass(target: WindowId, windows: Vec<WindowId>) -> FloatPass { FloatPass { target, windows } }

    #[test]
    fn ordered_is_least_recently_focused_first_with_unfocused_windows_at_the_bottom() {
        let mut stack = FloatStack::default();
        let (a, b, c, d) = (
            WindowId::new(1, 1),
            WindowId::new(1, 2),
            WindowId::new(2, 1),
            WindowId::new(2, 2),
        );
        stack.note_focus(c);
        stack.note_focus(a);
        stack.note_focus(c);
        assert_eq!(stack.ordered(vec![a, b, c, d]), vec![b, d, a, c]);
        // Re-focusing moves a window to the top; the list stays bounded.
        stack.note_focus(a);
        assert_eq!(stack.ordered(vec![c, a]), vec![c, a]);
        for i in 0..(FOCUS_ORDER_LIMIT as u32 + 10) {
            stack.note_focus(WindowId::new(9, i + 1));
        }
        assert_eq!(stack.focus_order.len(), FOCUS_ORDER_LIMIT);
    }

    #[test]
    fn echoes_of_a_sent_pass_are_not_admitted_until_focus_moved_or_a_second_passed() {
        let mut stack = FloatStack::default();
        let (t, f) = (WindowId::new(1, 1), WindowId::new(2, 1));
        let now = Instant::now();
        let p = pass(t, vec![f]);
        assert!(stack.admit(&p, now));
        assert!(!stack.admit(&p, now + Duration::from_millis(100)), "echo");
        // A different float set for the same target is a new pass.
        let p2 = pass(t, vec![f, WindowId::new(3, 1)]);
        assert!(stack.admit(&p2, now + Duration::from_millis(200)));
        // Focus moved to the float and back: the same pass is wanted again.
        stack.note_focus(f);
        stack.note_focus(t);
        assert!(stack.admit(&p2, now + Duration::from_millis(300)));
        // Re-focusing the target itself (echo) does not reset the echo window...
        stack.note_focus(t);
        assert!(!stack.admit(&p2, now + Duration::from_millis(400)));
        // ...but after a second the pass is admitted again.
        assert!(stack.admit(&p2, now + Duration::from_millis(1400)));
    }

    #[test]
    fn rate_limit_backs_off_after_too_many_passes_in_a_second() {
        let mut stack = FloatStack::default();
        let f = WindowId::new(9, 1);
        let now = Instant::now();
        let mut admitted = 0;
        for i in 0..40u32 {
            // Alternate targets so no pass is an echo of the previous one.
            let target = WindowId::new(1, i % 2 + 1);
            stack.note_focus(target);
            if stack.admit(
                &pass(target, vec![f]),
                now + Duration::from_millis(10 * i as u64),
            ) {
                admitted += 1;
            }
        }
        assert_eq!(admitted, MAX_PASSES_PER_SECOND);
        assert!(stack.backoff_until.is_some());
        // The limit tripped at 80 ms: still backing off at 1 s, admitted again at 1.5 s.
        let target = WindowId::new(1, 1);
        stack.note_focus(target);
        assert!(!stack.admit(&pass(target, vec![f]), now + Duration::from_millis(1000)));
        assert!(stack.admit(&pass(target, vec![f]), now + Duration::from_millis(1500)));
        assert!(stack.backoff_until.is_none());
    }

    #[test]
    fn app_activating_itself_after_a_pass_is_excluded_unless_rift_raised_it() {
        let mut stack = FloatStack::default();
        let (t, f, g) = (WindowId::new(1, 1), WindowId::new(2, 1), WindowId::new(3, 1));
        let now = Instant::now();
        assert!(stack.admit(&pass(t, vec![f, g]), now));
        // The target's own activation is the raise rift asked for.
        assert!(!stack.note_app_activated(t.pid, now + Duration::from_millis(50)));
        // An app whose float was raised and that rift did not ask to raise: self-activation.
        assert!(stack.note_app_activated(f.pid, now + Duration::from_millis(100)));
        assert!(stack.is_excluded(f.pid, now + Duration::from_millis(100)));
        assert!(
            !stack.is_excluded(f.pid, now + SELF_ACTIVATION_EXCLUSION + Duration::from_secs(1))
        );
        // Rift raised g's app meanwhile (a queued focus raise): not a self-activation.
        stack.note_raise([g.pid], now + Duration::from_millis(150));
        assert!(!stack.note_app_activated(g.pid, now + Duration::from_millis(200)));
        assert!(!stack.is_excluded(g.pid, now + Duration::from_millis(200)));
        // Too late to be a response to the pass.
        assert!(!stack.note_app_activated(g.pid, now + Duration::from_secs(3)));
        // An app not in the pass is never attributed.
        assert!(!stack.note_app_activated(77, now + Duration::from_millis(100)));
        stack.forget_app(f.pid);
        assert!(!stack.is_excluded(f.pid, now + Duration::from_millis(100)));
    }

    #[test]
    fn pending_focus_change_is_taken_once_and_only_for_its_target() {
        let mut stack = FloatStack::default();
        let space = SpaceId::new(1);
        let (t, u) = (WindowId::new(1, 1), WindowId::new(1, 2));
        stack.queue(space, t);
        assert_eq!(stack.take_pending_for(u), None);
        assert_eq!(stack.take_pending_for(t), Some(space));
        assert_eq!(stack.take_pending(), None);
        stack.queue(space, t);
        assert_eq!(stack.take_pending(), Some((space, t)));
        assert_eq!(stack.take_pending(), None);
    }
}
