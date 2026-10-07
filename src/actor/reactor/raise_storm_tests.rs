//! Two displays: authoritative space snapshots must not move focus between displays.
//!
//! Setup: two displays with separate Spaces, each showing one window in its active virtual
//! workspace (Chrome on the built-in, Firefox on the external display), plus windows parked in
//! inactive workspaces on both displays. A third-party window that follows the active menu bar
//! (e.g. Bartender's menu bar cover) makes every active-display change an authoritative snapshot
//! whose `active_window_spaces` differs, so the reactor takes the full reconcile path, which
//! re-sends `WindowAdded` for every visible window, parked ones included.
//!
//! Regression: every such snapshot armed one refocus per parked window, i.e. a focus raise of
//! the visible window on that window's display, alternating displays in window-server-id order.
//! Each cross-display activation moved the menu bar (and the cover window) and caused the next
//! snapshot, so the raise queue never drained: focus ping-ponged between the displays ~9x/s.
//!
//! The harness stands in for the raise manager and WindowServer: focus raises run FIFO; raising
//! a window of a non-frontmost app activates it, and when that window sits on the other display
//! the menu bar (and the cover window) move there.
use std::collections::VecDeque;

use objc2_core_foundation::{CGPoint, CGRect, CGSize};

use super::testing::*;
use super::*;
use crate::actor::app::pid_t;
use crate::common::collections::BTreeMap;
use crate::common::config::{AppWorkspaceRule, VirtualWorkspaceSettings, WorkspaceScope};
use crate::layout_engine::{LayoutCommand, LayoutEvent};
use crate::sys::app::WindowInfo;
use crate::sys::window_server::{self, WindowServerId};

const CHROME: pid_t = 400_001;
const FIREFOX: pid_t = 400_002;
/// Parked windows get one app each (pid PARKED_PID + i), so their wsids sort in spec order.
const PARKED_PID: pid_t = 400_100;
/// Parked windows of the reported incident in wsid order: B = built-in, E = external display.
const INCIDENT_PARKED: &str = "BEEBBBBBEBBB";
/// The menu bar cover window: not tracked by rift, follows the active menu bar.
const COVER: u32 = 4_100_000_001;

fn rect(x: f64, y: f64, w: f64, h: f64) -> CGRect {
    CGRect::new(CGPoint::new(x, y), CGSize::new(w, h))
}

/// Mirrors `Apps::make_app_with_opts` (synthetic wsid = pid * 10_000 + idx).
fn wsid_of(wid: WindowId) -> WindowServerId {
    WindowServerId::new((wid.pid as u32) * 10_000 + wid.idx.get())
}

fn focus_targets(raises: &[RaiseRequest]) -> Vec<WindowId> {
    raises
        .iter()
        .filter_map(|request| request.focus_window.map(|(wid, _)| wid))
        .collect()
}

struct Storm {
    apps: Apps,
    reactor: Reactor,
    raise_rx: actor::Receiver<raise_manager::Event>,
    screens: Vec<CGRect>,
    builtin: SpaceId,
    external: SpaceId,
    native_space: BTreeMap<WindowServerId, SpaceId>,
    chrome: WindowId,
    firefox: WindowId,
    parked: Vec<WindowId>,
    menu: SpaceId,
    front: pid_t,
}

#[derive(Debug, Default)]
struct Tally {
    focus_raises: usize,
    noop_raises: usize,
    activations: usize,
    display_flips: usize,
    queue_left: usize,
}

impl Storm {
    /// `parked`: one char per window parked in an inactive workspace, in wsid order;
    /// 'B' = built-in, 'E' = external display. `parked_floating`: parked windows float.
    fn new(parked: &str, parked_floating: bool) -> Storm {
        Self::with_scope(parked, parked_floating, false)
    }

    /// `global`: one set of workspaces shared by both displays (four of them: the built-in
    /// shows ws0 and parks in ws1, the external shows ws2 and parks in ws3) instead of a set
    /// per display (three each: ws0 shown, ws1 parked).
    fn with_scope(parked: &str, parked_floating: bool, global: bool) -> Storm {
        window_server::set_test_no_cursor_window(true);
        window_server::set_space_window_list_for_connection_override(Some(vec![]));
        let builtin = SpaceId::new(1);
        let external = SpaceId::new(708);
        let rules = (0..parked.len()).filter(|_| parked_floating).map(|i| AppWorkspaceRule {
            app_id: Some(format!("com.testapp{}", PARKED_PID + i as pid_t)),
            floating: true,
            ..Default::default()
        });
        let settings = VirtualWorkspaceSettings {
            default_workspace_count: if global { 4 } else { 3 },
            scope: if global {
                WorkspaceScope::Global
            } else {
                WorkspaceScope::PerDisplay
            },
            app_rules: rules.collect(),
            ..Default::default()
        };
        let mut reactor = test_reactor_with_workspace_settings(&settings);
        reactor.config.virtual_workspaces = settings;
        let (raise_tx, raise_rx) = actor::channel();
        reactor.communication_manager.raise_manager_tx = raise_tx;
        // Built-in and a portrait display left of it (menu bars excluded).
        let screens = vec![
            rect(0., 40., 1800., 1129.),
            rect(-1440., -975., 1440., 2529.),
        ];
        let mut storm = Storm {
            apps: Apps::new(),
            reactor,
            raise_rx,
            screens,
            builtin,
            external,
            native_space: BTreeMap::new(),
            chrome: WindowId::new(CHROME, 1),
            firefox: WindowId::new(FIREFOX, 1),
            parked: vec![],
            menu: builtin,
            front: CHROME,
        };
        let snapshot = storm.snapshot(builtin, None);
        storm.reactor.handle_event(snapshot);
        // Which workspace each display shows and which it parks in.
        let workspaces_of = |space: SpaceId| -> (usize, usize) {
            if global && space == external {
                (2, 3)
            } else {
                (0, 1)
            }
        };
        if global {
            let ws2 = storm.reactor.test_workspace(external, 2);
            assert!(storm.reactor.set_test_active_workspace(external, ws2));
        }

        // Launch each parked window while its display shows the parking workspace, then go
        // back to the shown one.
        for (i, display) in parked.chars().enumerate() {
            let pid = PARKED_PID + i as pid_t;
            let (space, frame) = match display {
                'B' => (builtin, rect(5., 40., 890., 1124.)),
                'E' => (external, rect(-1435., -975., 1430., 1260.)),
                other => panic!("bad display {other:?}"),
            };
            let (shown, hidden) = workspaces_of(space);
            let park = storm.reactor.test_workspace(space, hidden);
            assert!(storm.reactor.set_test_active_workspace(space, park));
            storm.parked.push(WindowId::new(pid, 1));
            storm.launch(pid, space, vec![make_window_info(frame, None, "parked", None)]);
            let show = storm.reactor.test_workspace(space, shown);
            assert!(storm.reactor.set_test_active_workspace(space, show));
        }
        storm.launch(CHROME, builtin, vec![make_window_info(
            rect(5., 40., 1790., 1124.),
            None,
            "Chrome",
            None,
        )]);
        storm.launch(FIREFOX, external, vec![make_window_info(
            rect(-1435., -975., 1430., 2524.),
            None,
            "Firefox",
            None,
        )]);
        storm.reactor.update_layout_or_warn(false, false, None);
        storm.apps.simulate_until_quiet(&mut storm.reactor);

        // Each display's visible window is its active workspace's selection; Chrome has focus.
        let (chrome, firefox) = (storm.chrome, storm.firefox);
        storm.reactor.send_layout_event(LayoutEvent::WindowFocused(external, firefox));
        storm.reactor.handle_event(Event::ApplicationGloballyActivated(CHROME));
        storm.apps.simulate_until_quiet(&mut storm.reactor);
        storm.reactor.handle_event(Event::WindowServerFocusChanged(chrome, builtin));
        storm.apps.simulate_until_quiet(&mut storm.reactor);

        assert_eq!(storm.reactor.test_active_workspace_windows(builtin), vec![
            chrome
        ]);
        assert_eq!(storm.reactor.test_active_workspace_windows(external), vec![
            firefox
        ]);
        for &wid in &storm.parked {
            assert!(
                storm.reactor.state.windows.is_visible_admitted(wid),
                "{wid:?}: parked windows stay natively visible (moved, not ordered out)"
            );
            assert_eq!(
                storm.reactor.layout_manager.layout_engine.is_window_floating(wid),
                parked_floating,
                "{wid:?}"
            );
            assert!(
                !storm.reactor.test_active_workspace_windows(storm.space_of(wid)).contains(&wid),
                "{wid:?} is parked in an inactive workspace"
            );
        }
        storm.drain_raises();
        storm
    }

    fn launch(&mut self, pid: pid_t, space: SpaceId, windows: Vec<WindowInfo>) {
        for idx in 1..=windows.len() as u32 {
            let wsid = wsid_of(WindowId::new(pid, idx));
            window_server::set_window_spaces_override(wsid, Some(vec![space.get()]));
            self.native_space.insert(wsid, space);
        }
        let main = Some(WindowId::new(pid, 1));
        let events = self.apps.make_app_with_opts(pid, windows, main, false, true);
        self.reactor.handle_events(events);
        self.apps.simulate_until_quiet(&mut self.reactor);
    }

    /// Authoritative snapshot as forwarded by the spaces actor. `cover`: space of the menu bar
    /// cover window (None = no such window).
    fn snapshot(&self, menu: SpaceId, cover: Option<SpaceId>) -> Event {
        let native = self.native_space.clone();
        space_state_event_with(
            self.screens.clone(),
            vec![Some(self.builtin), Some(self.external)],
            |state| {
                state.membership_complete = true;
                state.menu_bar_space = Some(menu);
                state.command_space = Some(menu);
                state.active_window_spaces.clear();
                state.active_window_spaces.extend(native);
                if let Some(space) = cover {
                    state.active_window_spaces.insert(WindowServerId::new(COVER), space);
                }
            },
        )
    }

    fn drain_raises(&mut self) -> Vec<RaiseRequest> {
        let mut requests = vec![];
        while let Ok((_, event)) = self.raise_rx.try_recv() {
            if let raise_manager::Event::RaiseRequest(request) = event {
                requests.push(request);
            }
        }
        requests
    }

    fn space_of(&self, wid: WindowId) -> SpaceId { self.native_space[&wsid_of(wid)] }

    /// A workspace command as the user's hotkey sends it, acting on the display with focus.
    fn command(&mut self, command: LayoutCommand) {
        self.reactor.handle_test_layout_command(command);
        self.apps.simulate_until_quiet(&mut self.reactor);
    }

    /// One authoritative snapshot (as after an active-display change), then execute focus raises
    /// FIFO like the raise manager, feeding the resulting activations back.
    fn run(&mut self, cover_follows_menu_bar: bool, max_raises: usize) -> Tally {
        let cover = |menu| cover_follows_menu_bar.then_some(menu);
        let snapshot = self.snapshot(self.menu, cover(self.menu));
        self.reactor.handle_event(snapshot);
        self.apps.simulate_until_quiet(&mut self.reactor);
        self.drive(cover_follows_menu_bar, max_raises)
    }

    /// Execute the queued focus raises FIFO like the raise manager, feeding the resulting
    /// activations (and, on a cross-display one, the menu bar move) back.
    fn drive(&mut self, cover_follows_menu_bar: bool, max_raises: usize) -> Tally {
        let mut tally = Tally::default();
        let cover = |menu| cover_follows_menu_bar.then_some(menu);
        let mut queue: VecDeque<RaiseRequest> = self.drain_raises().into();
        while let Some(request) = queue.pop_front() {
            if tally.focus_raises == max_raises {
                tally.queue_left = queue.len() + 1;
                break;
            }
            let Some((wid, _warp)) = request.focus_window else {
                continue;
            };
            tally.focus_raises += 1;
            if self.front == wid.pid {
                // Frontmost app's main window: the raise completes without activating anything.
                tally.noop_raises += 1;
            } else {
                // make_key_window (SetFrontProcess) + AXRaise -> activation notifications.
                tally.activations += 1;
                self.front = wid.pid;
                self.reactor.handle_event(Event::ApplicationGloballyActivated(wid.pid));
                self.apps.simulate_until_quiet(&mut self.reactor);
                let space = self.space_of(wid);
                self.reactor.handle_event(Event::WindowServerFocusChanged(wid, space));
                if space != self.menu {
                    // Key window on the other display: menu bar (and its cover) move there.
                    tally.display_flips += 1;
                    self.menu = space;
                    let snapshot = self.snapshot(space, cover(space));
                    self.reactor.handle_event(snapshot);
                }
                self.apps.simulate_until_quiet(&mut self.reactor);
            }
            queue.extend(self.drain_raises());
        }
        tally
    }
}

/// A snapshot that only re-confirms which Space each window is on must not refocus anything.
/// Before the fix: one focus raise per parked window (12 here), alternating displays.
#[test]
fn snapshot_reconfirming_parked_windows_does_not_refocus() {
    for parked_floating in [false, true] {
        let mut storm = Storm::new(INCIDENT_PARKED, parked_floating);
        let snapshot = storm.snapshot(storm.external, Some(storm.external));
        storm.reactor.handle_event(snapshot);
        storm.apps.simulate_until_quiet(&mut storm.reactor);
        let focus = focus_targets(&storm.drain_raises());
        assert!(
            focus.is_empty(),
            "floating={parked_floating}: snapshot reconcile raised {} windows: {focus:?}",
            focus.len()
        );
        assert!(matches!(
            storm.reactor.refocus_manager.refocus_state,
            RefocusState::None
        ));
    }
}

/// The incident shape (parked windows on both displays, a window that follows the menu bar):
/// focus must not ping-pong between the displays. Before the fix the raise queue never drained
/// (600 raises: 200 cross-display activations, queue still growing).
#[test]
fn two_displays_with_parked_windows_do_not_ping_pong_focus() {
    let cases = [
        (INCIDENT_PARKED, false, true),
        (INCIDENT_PARKED, true, true),
        ("BE", false, true),
        ("BE", true, true),
        (INCIDENT_PARKED, false, false),
    ];
    for (parked, parked_floating, cover) in cases {
        let mut storm = Storm::new(parked, parked_floating);
        let tally = storm.run(cover, 600);
        let case = format!("parked={parked} floating={parked_floating} cover={cover}");
        assert_eq!(tally.activations, 0, "{case}: {tally:?}");
        assert_eq!(tally.display_flips, 0, "{case}: {tally:?}");
        assert_eq!(tally.queue_left, 0, "{case}: {tally:?}");
        assert_eq!(storm.front, CHROME, "{case}: focus stays on the built-in");
    }
}

/// The refocus the fix keeps: a parked window that was taken out of its workspace (e.g.
/// minimized) and comes back is not a re-confirmation; adding it back to its hidden workspace
/// still hands focus to the visible window of its display.
#[test]
fn window_returning_to_hidden_workspace_still_refocuses() {
    for parked_floating in [false, true] {
        let mut storm = Storm::new("BE", parked_floating);
        let (builtin, parked) = (storm.builtin, storm.parked[0]);
        storm
            .reactor
            .send_layout_event(LayoutEvent::WindowRemovedPreserveFloating(parked));
        storm.apps.simulate_until_quiet(&mut storm.reactor);
        assert!(focus_targets(&storm.drain_raises()).is_empty());

        // The next snapshot's reconcile re-adds it to its workspace.
        let snapshot = storm.snapshot(builtin, Some(builtin));
        storm.reactor.handle_event(snapshot);
        storm.apps.simulate_until_quiet(&mut storm.reactor);
        let ws1 = storm.reactor.test_workspace(builtin, 1);
        assert!(storm.reactor.test_workspace_windows(builtin, ws1).contains(&parked));
        let focus = focus_targets(&storm.drain_raises());
        assert_eq!(focus, vec![storm.chrome], "floating={parked_floating}");
    }
}

/// Defence in depth: a refocus armed for a display that does not have focus must not activate
/// that display's window (it would move the active display and the menu bar).
#[test]
fn refocus_on_display_without_focus_does_not_raise() {
    let mut storm = Storm::new("BE", false);
    let (builtin, external, chrome) = (storm.builtin, storm.external, storm.chrome);
    for (space, expected) in [(external, vec![]), (builtin, vec![chrome])] {
        storm.reactor.refocus_manager.refocus_state = RefocusState::Pending(space);
        storm.reactor.handle_layout_response(layout::EventResponse::default(), None);
        assert_eq!(focus_targets(&storm.drain_raises()), expected, "{space:?}");
    }

    // Through the reconcile path: the external display's parked window comes back to its hidden
    // workspace while the built-in has focus.
    let parked = storm.parked[1];
    assert_eq!(storm.space_of(parked), external);
    storm
        .reactor
        .send_layout_event(LayoutEvent::WindowRemovedPreserveFloating(parked));
    storm.apps.simulate_until_quiet(&mut storm.reactor);
    let snapshot = storm.snapshot(builtin, Some(builtin));
    storm.reactor.handle_event(snapshot);
    storm.apps.simulate_until_quiet(&mut storm.reactor);
    let ws1 = storm.reactor.test_workspace(external, 1);
    assert!(storm.reactor.test_workspace_windows(external, ws1).contains(&parked));
    assert_eq!(focus_targets(&storm.drain_raises()), vec![]);
}

/// With shared workspaces every remote `alt-N` focuses the other display, the very move that
/// sustained the storm. Each such switch must cost one activation and one menu-bar flip and then
/// leave the raise queue empty, with parked windows on both displays and the menu bar cover
/// present: no ping-pong, however often the user switches back and forth.
#[test]
fn global_workspaces_remote_switches_do_not_storm() {
    for parked_floating in [false, true] {
        let mut storm = Storm::with_scope(INCIDENT_PARKED, parked_floating, true);
        let (builtin, external) = (storm.builtin, storm.external);
        assert_eq!(storm.reactor.space_state.command_space, Some(builtin));
        // ws2 shows on the external display, ws3 is parked there; ws0 shows on the built-in
        // and ws1 is parked there. The user alternates between the displays.
        let switches = [2, 0, 3, 1, 2, 1, 2, 0];
        for (step, index) in switches.into_iter().enumerate() {
            let display = if index >= 2 { external } else { builtin };
            storm.command(LayoutCommand::SwitchToWorkspace(index));
            let tally = storm.drive(true, 200);
            let case = format!("floating={parked_floating} step={step} alt-{index}: {tally:?}");
            assert_eq!(tally.queue_left, 0, "{case}");
            assert!(tally.activations <= 1, "{case}");
            assert!(tally.display_flips <= 1, "{case}");
            // Any further raise targets the app already in front. (Hidden floating windows
            // still arm one such no-op refocus each per full snapshot after a switch: the
            // re-confirmation check cannot tell a parked floating window from a returning
            // one, in either scope.)
            assert_eq!(
                tally.noop_raises + tally.activations,
                tally.focus_raises,
                "{case}"
            );
            assert_eq!(storm.menu, display, "{case}: the menu bar follows the switch");
            assert_eq!(
                storm.reactor.space_state.command_space,
                Some(display),
                "{case}: commands act on the display switched to"
            );
            assert_eq!(
                storm.space_of(WindowId::new(storm.front, 1)),
                display,
                "{case}: the front app is on the display switched to"
            );
            let active = storm
                .reactor
                .layout_manager
                .layout_engine
                .workspaces()
                .active_workspace_idx(display);
            assert_eq!(active, Some(index as u64), "{case}");
        }
        // Everything is where it started: nothing moved between the displays.
        for &wid in &storm.parked {
            assert!(
                !storm.reactor.test_active_workspace_windows(storm.space_of(wid)).contains(&wid)
            );
        }
        assert_eq!(storm.reactor.test_active_workspace_windows(builtin), vec![
            storm.chrome
        ]);
        assert_eq!(storm.reactor.test_active_workspace_windows(external), vec![
            storm.firefox
        ]);
    }
}
