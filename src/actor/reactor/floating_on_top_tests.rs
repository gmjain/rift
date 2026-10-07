//! Floating windows stay above tiled ones (`settings.floating_windows_on_top`).
//!
//! On the two-display `Storm` harness (see `raise_storm_tests`): a focus change to a tiled
//! window, whether rift's own raise or one macOS reports (a click), is followed by one float
//! pass that restacks the visible floating windows of that window's workspace above it, least
//! recently focused first, without activating their apps. A floating focus passes nothing,
//! other displays' and parked floats stay out, fullscreen suppresses passes, the events a pass
//! echoes do not repeat it, an app that activates itself when restacked is bounded, and the
//! incident's two-display switching with floats on both displays does not storm.
use super::raise_storm_tests::{FLOAT_PID, INCIDENT_PARKED, Storm, StormSpec, focus_targets};
use super::*;
use crate::actor::reactor::float_stack::MAX_PASSES_PER_SECOND;
use crate::layout_engine::{Direction, LayoutCommand};
use crate::sys::window_server::WindowServerId;

fn restacks(raises: &[RaiseRequest]) -> Vec<Vec<WindowId>> {
    raises
        .iter()
        .filter(|request| !request.restack_windows.is_empty())
        .map(|request| request.restack_windows.clone())
        .collect()
}

fn spec(floats: &str) -> StormSpec<'_> {
    StormSpec {
        floats,
        tiled2: true,
        floating_on_top: true,
        ..Default::default()
    }
}

/// A focus change to a tiled window restacks the workspace's floats above it, least recently
/// focused first: as a request of its own (restacks only, nothing raised or focused) when
/// macOS reports the change, and appended to rift's own raise sequence after the focus window
/// when the change is rift's.
#[test]
fn tiled_focus_restacks_the_workspace_floats_least_recently_focused_first() {
    let mut storm = Storm::from_spec(spec("BBB"));
    let (chrome, tiled2) = (storm.chrome, storm.tiled2);
    let [f0, f1, f2] = storm.floats[..] else { panic!() };
    // Focus order: f1 then f0 were focused, f2 never.
    storm.click(f1, false);
    storm.click(f0, false);
    assert_eq!(restacks(&storm.drain_raises()), Vec::<Vec<WindowId>>::new());

    // A click on the tiled window (a focus change macOS reports).
    storm.click(chrome, false);
    let raises = storm.drain_raises();
    assert_eq!(raises.len(), 1, "{raises:?}");
    assert_eq!(raises[0].focus_window, None);
    assert!(raises[0].raise_windows.is_empty());
    assert_eq!(raises[0].restack_windows, vec![f2, f1, f0]);
    for float in [f0, f1, f2] {
        assert!(raises[0].app_handles.contains_key(&float.pid));
    }

    // A focus command (rift's own raise): one sequence, floats after the focus window.
    storm.command(LayoutCommand::MoveFocus(Direction::Right));
    let raises = storm.drain_raises();
    assert_eq!(raises.len(), 1, "{raises:?}");
    assert_eq!(focus_targets(&raises), vec![tiled2]);
    assert_eq!(raises[0].restack_windows, vec![f2, f1, f0]);
    assert!(raises[0].app_handles.contains_key(&tiled2.pid));
    for float in [f0, f1, f2] {
        assert!(raises[0].app_handles.contains_key(&float.pid));
    }

    // Focusing a float moves it to the top of the order.
    storm.click(f2, false);
    storm.click(chrome, false);
    assert_eq!(restacks(&storm.drain_raises()), vec![vec![f1, f0, f2]]);
}

/// Focus on a floating window passes nothing, by click or by command.
#[test]
fn floating_focus_passes_nothing() {
    let mut storm = Storm::from_spec(spec("BB"));
    let chrome = storm.chrome;
    let [f0, f1] = storm.floats[..] else { panic!() };
    storm.click(f0, false);
    assert!(storm.drain_raises().is_empty());

    storm.click(chrome, false);
    assert_eq!(restacks(&storm.drain_raises()), vec![vec![f1, f0]]);
    storm.command(LayoutCommand::ToggleFocusFloating);
    let raises = storm.drain_raises();
    assert_eq!(raises.len(), 1, "{raises:?}");
    assert!(
        storm
            .reactor
            .layout_manager
            .layout_engine
            .is_window_floating(focus_targets(&raises)[0])
    );
    assert!(raises[0].restack_windows.is_empty());
}

/// Only the floats of the focused window's own display and active workspace are restacked:
/// the other display's floats and parked (hidden workspace) floats stay out.
#[test]
fn other_display_and_parked_floats_stay_out() {
    let mut storm = Storm::from_spec(StormSpec {
        parked: "BE",
        parked_floating: true,
        floats: "BE",
        floating_on_top: true,
        ..Default::default()
    });
    let (chrome, firefox) = (storm.chrome, storm.firefox);
    let [float_b, float_e] = storm.floats[..] else { panic!() };
    storm.click(float_b, false);
    storm.click(chrome, false);
    assert_eq!(restacks(&storm.drain_raises()), vec![vec![float_b]]);
    storm.click(firefox, false);
    assert_eq!(restacks(&storm.drain_raises()), vec![vec![float_e]]);
    storm.click(float_b, false);
    storm.click(chrome, false);
    assert_eq!(restacks(&storm.drain_raises()), vec![vec![float_b]]);
}

/// No pass while the focused tiled window is fullscreen (`toggle_fullscreen`) or its Space is
/// a native fullscreen Space.
#[test]
fn fullscreen_suppresses_passes() {
    let mut storm = Storm::from_spec(spec("B"));
    let (builtin, chrome) = (storm.builtin, storm.chrome);
    let f0 = storm.floats[0];
    let click_float_then_chrome = |storm: &mut Storm| {
        storm.click(f0, false);
        storm.click(chrome, false);
        restacks(&storm.drain_raises())
    };
    assert_eq!(click_float_then_chrome(&mut storm), vec![vec![f0]]);

    storm.command(LayoutCommand::ToggleFullscreen);
    assert!(
        storm
            .reactor
            .layout_manager
            .layout_engine
            .active_workspace_for_space_has_fullscreen(builtin)
    );
    assert!(restacks(&storm.drain_raises()).is_empty());
    assert!(click_float_then_chrome(&mut storm).is_empty());

    storm.command(LayoutCommand::ToggleFullscreen);
    storm.drain_raises();
    assert_eq!(click_float_then_chrome(&mut storm), vec![vec![f0]]);

    storm.reactor.space_state.fullscreen_spaces.insert(builtin);
    assert!(click_float_then_chrome(&mut storm).is_empty());
    storm.reactor.space_state.fullscreen_spaces.remove(&builtin);
    assert_eq!(click_float_then_chrome(&mut storm), vec![vec![f0]]);
}

/// The events a pass echoes (the target app's activation, the WindowServer focus report, a
/// snapshot re-sending the focused window) do not repeat it. An app that activates itself
/// when its float is restacked takes focus once, re-raises nothing, and is left out of the
/// following passes.
#[test]
fn echoes_do_not_repeat_a_pass_and_self_activation_is_bounded() {
    let mut storm = Storm::from_spec(spec("BB"));
    let (builtin, chrome) = (storm.builtin, storm.chrome);
    let [f0, f1] = storm.floats[..] else { panic!() };
    storm.click(f1, false);
    storm.click(chrome, false);
    assert_eq!(restacks(&storm.drain_raises()), vec![vec![f0, f1]]);

    // Echoes of the pass.
    storm.reactor.handle_event(Event::ApplicationGloballyActivated(chrome.pid));
    storm.apps.simulate_until_quiet(&mut storm.reactor);
    storm.reactor.handle_event(Event::WindowServerFocusChanged(chrome, builtin));
    storm.apps.simulate_until_quiet(&mut storm.reactor);
    let snapshot = storm.snapshot(builtin, Some(builtin));
    storm.reactor.handle_event(snapshot);
    storm.apps.simulate_until_quiet(&mut storm.reactor);
    assert!(storm.drain_raises().is_empty(), "echoes of the pass");

    // f1's app activates itself on the restack: focus goes to f1, nothing is raised, and
    // f1's app is left out of the next passes.
    storm.self_activate(f1, false);
    assert!(storm.drain_raises().is_empty());
    assert_eq!(
        storm.reactor.layout_manager.layout_engine.focused_window(),
        Some(f1)
    );
    let now = std::time::Instant::now();
    assert!(storm.reactor.float_stack.is_excluded(f1.pid, now));
    assert!(!storm.reactor.float_stack.is_excluded(f0.pid, now));
    storm.click(chrome, false);
    assert_eq!(restacks(&storm.drain_raises()), vec![vec![f0]]);

    // A user click on a float right after a pass is not a self-activation.
    storm.click(f0, false);
    assert!(storm.drain_raises().is_empty());
    assert!(!storm.reactor.float_stack.is_excluded(f0.pid, std::time::Instant::now()));
    storm.click(chrome, false);
    assert_eq!(restacks(&storm.drain_raises()), vec![vec![f0]]);
}

/// Many focus changes in a second are allowed `MAX_PASSES_PER_SECOND` passes, then none.
#[test]
fn passes_are_rate_limited() {
    let mut storm = Storm::from_spec(spec("B"));
    let (chrome, tiled2) = (storm.chrome, storm.tiled2);
    let mut passes = 0;
    for _ in 0..20 {
        storm.click(chrome, false);
        storm.click(tiled2, false);
        passes += restacks(&storm.drain_raises()).len();
    }
    // The setup's own pass (Chrome focused) may count against the same second.
    assert!(
        (MAX_PASSES_PER_SECOND - 1..=MAX_PASSES_PER_SECOND).contains(&passes),
        "{passes} passes"
    );
}

/// With the setting off nothing is restacked, by click or by command.
#[test]
fn setting_off_passes_nothing() {
    let mut storm = Storm::from_spec(StormSpec {
        floating_on_top: false,
        ..spec("B")
    });
    let (chrome, tiled2) = (storm.chrome, storm.tiled2);
    let f0 = storm.floats[0];
    storm.click(f0, false);
    storm.click(chrome, false);
    assert!(storm.drain_raises().is_empty());
    storm.command(LayoutCommand::MoveFocus(Direction::Right));
    let raises = storm.drain_raises();
    assert_eq!(focus_targets(&raises), vec![tiled2]);
    assert!(restacks(&raises).is_empty());
}

/// The focused app's own dialogs (not managed, but tracked with a window id) are restacked
/// last, on top of the floats; its own floating windows are not (AXRaise would hand them key
/// focus within the app).
#[test]
fn focused_apps_dialogs_go_on_top_and_its_floats_stay_out() {
    let mut storm = Storm::from_spec(StormSpec {
        chrome_float: true,
        ..spec("B")
    });
    let (builtin, chrome, chrome_float) = (storm.builtin, storm.chrome, storm.chrome_float);
    let f0 = storm.floats[0];
    assert!(storm.reactor.layout_manager.layout_engine.is_window_floating(chrome_float));

    let dialog = WindowId::new(chrome.pid, 3);
    let dialog_wsid = WindowServerId::new(4_200_000_001);
    storm.reactor.add_test_window_with_manageability(
        dialog,
        dialog_wsid,
        Some(builtin),
        CGRect::new(CGPoint::new(500., 300.), CGSize::new(400., 200.)),
        false,
    );
    let window = storm.reactor.state.windows.window_mut(dialog).unwrap();
    window.info.ax_role = Some("AXWindow".into());
    window.info.ax_subrole = Some("AXDialog".into());
    assert!(!storm.reactor.state.windows.window(dialog).unwrap().is_admitted());

    storm.click(f0, false);
    storm.click(chrome, false);
    assert_eq!(restacks(&storm.drain_raises()), vec![vec![f0, dialog]]);

    // Hidden (ordered out) dialogs stay out.
    storm.reactor.state.windows.mark_window_hidden(dialog_wsid);
    storm.click(f0, false);
    storm.click(chrome, false);
    assert_eq!(restacks(&storm.drain_raises()), vec![vec![f0]]);
}

/// The incident's shape plus floats on both displays: with shared workspaces every remote
/// `alt-N` focuses the other display and restacks its float, then the user clicks the tiled
/// window there. Each step costs at most one activation, one menu-bar flip and one float pass,
/// and the raise queue drains, with parked windows tiled or floating and with the external
/// float's app activating itself when restacked.
#[test]
fn remote_switches_and_clicks_with_floats_on_both_displays_do_not_storm() {
    for (parked_floating, self_activating) in [(false, false), (true, false), (true, true)] {
        let mut storm = Storm::from_spec(StormSpec {
            parked: INCIDENT_PARKED,
            parked_floating,
            global: true,
            floats: "BE",
            floating_on_top: true,
            ..Default::default()
        });
        let (builtin, external) = (storm.builtin, storm.external);
        let [float_b, float_e] = storm.floats[..] else { panic!() };
        assert_eq!(float_e.pid, FLOAT_PID + 1);
        if self_activating {
            storm.self_activating.insert(float_e.pid);
        }
        let switches = [2, 0, 3, 1, 2, 1, 2, 0];
        for (step, index) in switches.into_iter().enumerate() {
            let display = if index >= 2 { external } else { builtin };
            let case = format!(
                "parked_floating={parked_floating} self_activating={self_activating} \
                 step={step} alt-{index}"
            );
            storm.command(LayoutCommand::SwitchToWorkspace(index));
            let tally = storm.drive(true, 200);
            assert_eq!(tally.queue_left, 0, "{case}: {tally:?}");
            assert!(tally.activations <= 1, "{case}: {tally:?}");
            assert!(tally.display_flips <= 1, "{case}: {tally:?}");
            assert!(tally.passes <= 1, "{case}: {tally:?}");
            assert!(tally.restacks <= 1, "{case}: {tally:?}");
            assert!(tally.self_activations <= 1, "{case}: {tally:?}");
            assert_eq!(storm.menu, display, "{case}: the menu bar follows the switch");

            // A click on the display's tiled window (after a self-activation this is what
            // brings focus back from the float).
            let tiled = if display == external {
                storm.firefox
            } else {
                storm.chrome
            };
            storm.click(tiled, true);
            let tally = storm.drive(true, 200);
            assert_eq!(tally.queue_left, 0, "{case} click: {tally:?}");
            assert_eq!(tally.activations, 0, "{case} click: {tally:?}");
            assert_eq!(tally.display_flips, 0, "{case} click: {tally:?}");
            assert!(tally.passes <= 1, "{case} click: {tally:?}");
            assert!(tally.self_activations <= 1, "{case} click: {tally:?}");
            assert_eq!(
                storm.front, tiled.pid,
                "{case} click: focus stays on the tiled window"
            );
            assert_eq!(storm.menu, display, "{case} click");
        }
        // Nothing moved between the displays.
        for &wid in storm.parked.iter().chain([&float_b, &float_e]) {
            let shown = storm.reactor.test_active_workspace_windows(storm.space_of(wid));
            assert_eq!(
                shown.contains(&wid),
                storm.floats.contains(&wid),
                "{wid:?}: floats shown, parked windows parked"
            );
        }
    }
}
