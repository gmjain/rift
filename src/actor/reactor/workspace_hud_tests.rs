//! What the reactor tells the workspace HUD actor, and when.
//!
//! One show per user action that switches workspaces, sent at the decision
//! (before the outcome's focus and raise work) with the workspace's name and
//! the display that shows it; nothing for rift's own passes (space snapshots,
//! display reconnects, settling) and nothing while the HUD is off.

use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use test_log::test;

use super::testing::*;
use super::*;
use crate::actor::workspace_hud::{Event as HudEvent, ShowRequest};
use crate::common::config::{VirtualWorkspaceSettings, WorkspaceScope, WorkspaceSelector};
use crate::layout_engine::{LayoutCommand, LayoutEvent};
use crate::model::reactor::Command;
use crate::model::workspace_hud::HudDisplay;
use crate::sys::window_server;

const LEFT: &str = "test-display-0";
const RIGHT: &str = "test-display-1";

fn enable_hud(reactor: &mut Reactor) -> actor::Receiver<HudEvent> {
    reactor.config.settings.ui.workspace_hud.enabled = true;
    let (tx, rx) = actor::channel();
    reactor.communication_manager.workspace_hud_tx = Some(tx);
    rx
}

fn drain(rx: &mut actor::Receiver<HudEvent>) -> Vec<HudEvent> {
    std::iter::from_fn(|| rx.try_recv().ok().map(|(_, event)| event)).collect()
}

fn take_shows(rx: &mut actor::Receiver<HudEvent>) -> Vec<ShowRequest> {
    drain(rx)
        .into_iter()
        .filter_map(|event| match event {
            HudEvent::Show(show) => Some(show),
            _ => None,
        })
        .collect()
}

fn texts(shows: &[ShowRequest]) -> Vec<&str> {
    shows.iter().map(|show| show.text.as_str()).collect()
}

/// The name of workspace `index` on `space`.
fn name(reactor: &mut Reactor, space: SpaceId, index: usize) -> String {
    reactor.layout_manager.layout_engine.workspaces_mut().list_workspaces(space)[index]
        .1
        .clone()
}

fn active_index(reactor: &Reactor, space: SpaceId) -> Option<usize> {
    reactor
        .layout_manager
        .layout_engine
        .workspaces()
        .active_workspace_idx(space)
        .map(|index| index as usize)
}

/// "Workspace <name>" for the workspace `space` shows now.
fn showing(reactor: &mut Reactor, space: SpaceId) -> String {
    let index = active_index(reactor, space).expect("an active workspace");
    format!("Workspace {}", name(reactor, space, index))
}

fn switch(reactor: &mut Reactor, command: LayoutCommand) {
    reactor.handle_test_layout_command(command)
}

/// One display, two windows of one app, HUD on.
fn one_display() -> (Apps, Reactor, actor::Receiver<HudEvent>, SpaceId) {
    let (mut apps, mut reactor) = test_context();
    let mut rx = enable_hud(&mut reactor);
    let space = SpaceId::new(1);
    apps.make_app_and_settle_on_screen(&mut reactor, left_screen(), space, 1, make_windows(2));
    assert!(take_shows(&mut rx).is_empty(), "starting up shows nothing");
    (apps, reactor, rx, space)
}

fn global_settings(count: usize) -> VirtualWorkspaceSettings {
    VirtualWorkspaceSettings {
        default_workspace_count: count,
        workspace_names: (0..count).map(|index| format!("ws{index}")).collect(),
        scope: WorkspaceScope::Global,
        ..Default::default()
    }
}

fn global_reactor(settings: VirtualWorkspaceSettings) -> Reactor {
    let mut reactor = test_reactor_with_workspace_settings(&settings);
    reactor.config.virtual_workspaces = settings;
    reactor
}

/// Two displays sharing four workspaces: the left one shows ws0 with two
/// windows (and has focus), the right one ws1.
fn two_global_displays() -> (Apps, Reactor, actor::Receiver<HudEvent>, SpaceId, SpaceId) {
    let mut reactor = global_reactor(global_settings(4));
    let mut rx = enable_hud(&mut reactor);
    let (left, right) = (SpaceId::new(1), SpaceId::new(2));
    connect_displays(&mut reactor, vec![left_screen(), right_screen()], vec![
        Some(left),
        Some(right),
    ]);
    let mut apps = Apps::new();
    apps.make_app_and_settle(&mut reactor, 1, make_windows(2));
    assert_eq!(reactor.space_state.command_space, Some(left));
    assert_eq!(active_index(&reactor, left), Some(0));
    assert_eq!(active_index(&reactor, right), Some(1));
    assert!(
        take_shows(&mut rx).is_empty(),
        "connecting displays shows nothing"
    );
    (apps, reactor, rx, left, right)
}

fn focus_display(reactor: &mut Reactor, space: SpaceId) {
    reactor.handle_event(Event::ActiveDisplayChanged {
        menu_bar_space: Some(space),
        command_space: Some(space),
    });
    assert_eq!(reactor.space_state.command_space, Some(space));
}

#[test]
fn switch_to_workspace_shows_one_hud_with_its_name_on_its_display() {
    let (mut apps, mut reactor, mut rx, space) = one_display();

    switch(&mut reactor, LayoutCommand::SwitchToWorkspace(2));

    let shows = take_shows(&mut rx);
    assert_eq!(shows.len(), 1, "{shows:?}");
    assert_eq!(active_index(&reactor, space), Some(2));
    assert_eq!(
        shows[0].text,
        format!("Workspace {}", name(&mut reactor, space, 2))
    );
    assert_eq!(shows[0].target, LEFT);
    assert_eq!(shows[0].focused.as_deref(), Some(LEFT));

    // The focus, raise and frame work that follows, and its echoes, add nothing.
    apps.simulate_until_quiet(&mut reactor);
    assert!(take_shows(&mut rx).is_empty());
}

#[test]
fn ipc_switches_show_the_hud_like_hotkeys() {
    let (mut apps, mut reactor, mut rx, space) = one_display();

    reactor.handle_ipc_command(Command::Layout(LayoutCommand::SwitchToWorkspace(3)));
    apps.simulate_until_quiet(&mut reactor);

    let shows = take_shows(&mut rx);
    assert_eq!(texts(&shows), vec![showing(&mut reactor, space).as_str()]);
    assert_eq!(active_index(&reactor, space), Some(3));
}

#[test]
fn next_prev_and_back_and_forth_each_show_the_workspace_they_land_on() {
    let (mut apps, mut reactor, mut rx, space) = one_display();
    let mut expected = Vec::new();

    for command in [
        LayoutCommand::NextWorkspace(None),
        LayoutCommand::NextWorkspace(None),
        LayoutCommand::PrevWorkspace(None),
        LayoutCommand::SwitchToLastWorkspace,
    ] {
        let before = active_index(&reactor, space);
        switch(&mut reactor, command.clone());
        apps.simulate_until_quiet(&mut reactor);
        assert_ne!(active_index(&reactor, space), before, "{command:?} switches");
        expected.push(showing(&mut reactor, space));
    }

    let shows = take_shows(&mut rx);
    assert_eq!(
        texts(&shows),
        expected.iter().map(String::as_str).collect::<Vec<_>>()
    );
    assert!(shows.iter().all(|show| show.target == LEFT));
}

#[test]
fn switching_to_the_workspace_already_shown_shows_nothing() {
    let (mut apps, mut reactor, mut rx, space) = one_display();
    assert!(!reactor.config.virtual_workspaces.workspace_auto_back_and_forth);
    let current = active_index(&reactor, space).unwrap();

    switch(&mut reactor, LayoutCommand::SwitchToWorkspace(current));
    apps.simulate_until_quiet(&mut reactor);

    assert!(take_shows(&mut rx).is_empty());
}

#[test]
fn move_window_shows_the_hud_only_when_it_follows() {
    let (mut apps, mut reactor, mut rx, space) = one_display();
    // The fixture focuses no window, so name the windows to move.
    let [first, second] = [1, 2].map(|idx| WindowId::new(1, idx));
    let workspace_of =
        |reactor: &Reactor, wid| reactor.state.windows.workspace_for_window(space, wid);
    let start = workspace_of(&reactor, first);

    switch(&mut reactor, LayoutCommand::MoveWindowToWorkspace {
        workspace: WorkspaceSelector::Index(1),
        follow: false,
        window_id: Some(first.idx.get()),
    });
    apps.simulate_until_quiet(&mut reactor);
    assert_ne!(workspace_of(&reactor, first), start, "the window moved");
    assert_eq!(active_index(&reactor, space), Some(0));
    assert!(
        take_shows(&mut rx).is_empty(),
        "a move without follow is not a switch"
    );

    switch(&mut reactor, LayoutCommand::MoveWindowToWorkspace {
        workspace: WorkspaceSelector::Index(2),
        follow: true,
        window_id: Some(second.idx.get()),
    });
    apps.simulate_until_quiet(&mut reactor);
    assert_eq!(active_index(&reactor, space), Some(2));
    let shows = take_shows(&mut rx);
    assert_eq!(texts(&shows), vec![
        format!("Workspace {}", name(&mut reactor, space, 2)).as_str()
    ]);
}

#[test]
fn rapid_switches_send_one_show_each_newest_last() {
    let (mut apps, mut reactor, mut rx, space) = one_display();

    for index in [1, 2, 3] {
        switch(&mut reactor, LayoutCommand::SwitchToWorkspace(index));
    }
    let expected: Vec<String> = (1..=3)
        .map(|index| format!("Workspace {}", name(&mut reactor, space, index)))
        .collect();
    let shows = take_shows(&mut rx);
    // The actor turns these into one card whose text is replaced in place
    // (`HudPresenter`); the reactor's part is one message per action, in order.
    assert_eq!(
        texts(&shows),
        expected.iter().map(String::as_str).collect::<Vec<_>>()
    );

    apps.simulate_until_quiet(&mut reactor);
    assert!(
        take_shows(&mut rx).is_empty(),
        "no duplicates once the switches settle"
    );
}

#[test]
fn a_disabled_hud_gets_no_shows() {
    let (mut apps, mut reactor, mut rx, _space) = one_display();
    reactor.config.settings.ui.workspace_hud.enabled = false;

    let event = Event::Command(Command::Layout(LayoutCommand::SwitchToWorkspace(1)));
    assert!(reactor.probe_workspace_hud(&event).is_none());
    reactor.handle_event(event);
    switch(&mut reactor, LayoutCommand::NextWorkspace(None));
    apps.simulate_until_quiet(&mut reactor);

    assert!(take_shows(&mut rx).is_empty());
}

#[test]
fn the_template_fills_name_index_and_display() {
    let (mut apps, mut reactor, mut rx, space) = one_display();
    reactor.config.settings.ui.workspace_hud.text = "{index}/{name}/{display}".to_string();

    switch(&mut reactor, LayoutCommand::SwitchToWorkspace(1));
    apps.simulate_until_quiet(&mut reactor);

    let shows = take_shows(&mut rx);
    // Test screens have no name: the display's position stands in.
    assert_eq!(texts(&shows), vec![
        format!("2/{}/Display 1", name(&mut reactor, space, 1)).as_str()
    ]);
}

#[test]
fn the_show_goes_out_at_the_decision_with_no_window_server_or_ax_work() {
    let (mut apps, mut reactor, mut rx, space) = one_display();
    // A non-empty target, so the outcome has frames to write and focus to move.
    switch(&mut reactor, LayoutCommand::MoveWindowToWorkspace {
        workspace: WorkspaceSelector::Index(1),
        follow: false,
        window_id: None,
    });
    apps.simulate_until_quiet(&mut reactor);
    let _ = apps.requests();

    let event = Event::Command(Command::Layout(LayoutCommand::SwitchToWorkspace(1)));
    let probe = reactor.probe_workspace_hud(&event).expect("a switch command is probed");
    let outcome = reactor.dispatch_workflow(event).expect("the switch dispatches");
    assert_eq!(active_index(&reactor, space), Some(1), "decided");
    assert!(take_shows(&mut rx).is_empty());
    let _ = apps.requests();

    let counts = || {
        (
            window_server::window_space_query_count(),
            window_server::window_order_query_count(),
            window_server::window_at_point_query_count(),
        )
    };
    let before = counts();
    reactor.announce_workspace_switch(probe);
    assert_eq!(counts(), before, "the HUD path queries no WindowServer state");
    assert!(apps.requests().is_empty(), "the HUD path sends no AX request");
    assert_eq!(take_shows(&mut rx).len(), 1, "sent before the outcome runs");

    // Only now does the switch's focus/raise/frame work go out.
    reactor.apply_event_outcome(outcome);
    apps.simulate_until_quiet(&mut reactor);
    assert!(take_shows(&mut rx).is_empty());
}

#[test]
fn an_app_activation_that_switches_workspace_shows_the_hud_once() {
    let (mut apps, mut reactor) = test_context();
    let mut rx = enable_hud(&mut reactor);
    let space = SpaceId::new(1);
    let activated = WindowId::new(2, 1);
    reactor.handle_event(space_state_event(vec![left_screen()], vec![Some(space)]));
    apps.make_app_and_settle(&mut reactor, 2, make_windows(2));
    reactor.send_layout_event(LayoutEvent::WindowFocused(space, activated));
    switch(&mut reactor, LayoutCommand::MoveWindowToWorkspace {
        workspace: WorkspaceSelector::Index(1),
        follow: false,
        window_id: None,
    });
    switch(&mut reactor, LayoutCommand::SwitchToWorkspace(0));
    apps.simulate_until_quiet(&mut reactor);
    let _ = take_shows(&mut rx);

    reactor.handle_event(Event::ApplicationGloballyActivated(activated.pid));
    reactor.handle_event(Event::ApplicationActivated(activated.pid, Quiet::No));

    assert_eq!(
        active_index(&reactor, space),
        Some(1),
        "the activation follows the window"
    );
    let shows = take_shows(&mut rx);
    assert_eq!(texts(&shows), vec![
        format!("Workspace {}", name(&mut reactor, space, 1)).as_str()
    ]);
    // Activating it again changes nothing and shows nothing.
    reactor.handle_event(Event::ApplicationActivated(activated.pid, Quiet::No));
    apps.simulate_until_quiet(&mut reactor);
    assert!(take_shows(&mut rx).is_empty());
}

#[test]
fn a_global_switch_to_a_workspace_another_display_shows_appears_there() {
    let (mut apps, mut reactor, mut rx, left, right) = two_global_displays();

    // ws1 already shows on the right display: the switch only moves focus there.
    switch(&mut reactor, LayoutCommand::SwitchToWorkspace(1));
    let decided = take_shows(&mut rx);
    apps.simulate_until_quiet(&mut reactor);

    assert_eq!(reactor.space_state.command_space, Some(right));
    assert_eq!(active_index(&reactor, right), Some(1));
    assert_eq!(active_index(&reactor, left), Some(0));
    assert_eq!(decided.len(), 1, "sent with the decision: {decided:?}");
    assert_eq!(decided[0].text, "Workspace ws1");
    assert_eq!(decided[0].target, RIGHT);
    assert_eq!(decided[0].focused.as_deref(), Some(LEFT));
    assert!(
        take_shows(&mut rx).is_empty(),
        "the later space snapshots add nothing"
    );

    // ws3 belongs to no display: it opens where the user now is.
    switch(&mut reactor, LayoutCommand::SwitchToWorkspace(3));
    apps.simulate_until_quiet(&mut reactor);
    let shows = take_shows(&mut rx);
    assert_eq!(texts(&shows), vec!["Workspace ws3"]);
    assert_eq!(shows[0].target, RIGHT);
    assert_eq!(shows[0].focused.as_deref(), Some(RIGHT));
}

#[test]
fn a_global_switch_routed_to_the_owning_display_shows_there() {
    let (mut apps, mut reactor, mut rx, left, right) = two_global_displays();
    // Park window 2 in ws2 on the left display: ws2 now lives there.
    switch(&mut reactor, LayoutCommand::MoveWindowToWorkspace {
        workspace: WorkspaceSelector::Index(2),
        follow: false,
        window_id: Some(2),
    });
    apps.simulate_until_quiet(&mut reactor);
    focus_display(&mut reactor, right);
    assert!(
        take_shows(&mut rx).is_empty(),
        "neither a move nor a display focus is a switch"
    );

    switch(&mut reactor, LayoutCommand::SwitchToWorkspace(2));
    apps.simulate_until_quiet(&mut reactor);

    assert_eq!(reactor.space_state.command_space, Some(left));
    assert_eq!(active_index(&reactor, left), Some(2));
    let shows = take_shows(&mut rx);
    assert_eq!(texts(&shows), vec!["Workspace ws2"]);
    assert_eq!(shows[0].target, LEFT);
    assert_eq!(shows[0].focused.as_deref(), Some(RIGHT));
}

#[test]
fn a_global_move_with_follow_shows_on_the_display_it_lands_on() {
    let (mut apps, mut reactor, mut rx, left, right) = two_global_displays();

    // ws1 shows on the right display: the window and focus go there.
    switch(&mut reactor, LayoutCommand::MoveWindowToWorkspace {
        workspace: WorkspaceSelector::Index(1),
        follow: true,
        window_id: Some(2),
    });
    let decided = take_shows(&mut rx);
    apps.simulate_until_quiet(&mut reactor);

    assert_eq!(reactor.space_state.command_space, Some(right));
    assert_eq!(active_index(&reactor, left), Some(0));
    assert_eq!(texts(&decided), vec!["Workspace ws1"]);
    assert_eq!(decided[0].target, RIGHT);
    assert!(take_shows(&mut rx).is_empty());
}

#[test]
fn rift_passes_never_show_the_hud() {
    let mut reactor = global_reactor(global_settings(4));
    let mut rx = enable_hud(&mut reactor);
    let (left, right) = (SpaceId::new(1), SpaceId::new(2));
    connect_displays(&mut reactor, vec![left_screen()], vec![Some(left)]);
    let mut apps = Apps::new();
    apps.make_app_and_settle(&mut reactor, 1, make_windows(1));
    switch(&mut reactor, LayoutCommand::SwitchToWorkspace(1));
    apps.simulate_until_quiet(&mut reactor);
    assert_eq!(take_shows(&mut rx).len(), 1, "the user's switch");

    // A display connects and starts on a workspace nobody owns.
    connect_displays(&mut reactor, vec![left_screen(), right_screen()], vec![
        Some(left),
        Some(right),
    ]);
    apps.simulate_until_quiet(&mut reactor);
    assert_eq!(active_index(&reactor, right), Some(2));
    // A snapshot that re-confirms the topology (the echo of a switch).
    reactor.handle_event(space_state_event(vec![left_screen(), right_screen()], vec![
        Some(left),
        Some(right),
    ]));
    // The display goes away again: its workspaces are re-homed.
    connect_displays(&mut reactor, vec![left_screen()], vec![Some(left)]);
    apps.simulate_until_quiet(&mut reactor);
    // And comes back.
    connect_displays(&mut reactor, vec![left_screen(), right_screen()], vec![
        Some(left),
        Some(right),
    ]);
    apps.simulate_until_quiet(&mut reactor);

    assert!(
        take_shows(&mut rx).is_empty(),
        "snapshots, reconnects and re-homing show nothing"
    );
}

#[test]
fn displays_are_published_once_per_change_with_their_visible_frames() {
    let (_apps, mut reactor) = test_context();
    let mut rx = enable_hud(&mut reactor);
    // The HUD actor needs the displays even while off, to build at enable.
    reactor.config.settings.ui.workspace_hud.enabled = false;
    let (left, right) = (SpaceId::new(1), SpaceId::new(2));
    let visible = CGRect::new(CGPoint::new(0.0, 25.0), CGSize::new(1000.0, 975.0));

    reactor.handle_event(space_state_event(vec![visible], vec![Some(left)]));
    reactor.handle_event(space_state_event(vec![visible], vec![Some(left)]));
    reactor.handle_event(space_state_event(vec![visible, right_screen()], vec![
        Some(left),
        Some(right),
    ]));

    let published: Vec<Vec<HudDisplay>> = drain(&mut rx)
        .into_iter()
        .filter_map(|event| match event {
            HudEvent::DisplaysChanged(displays) => Some(displays),
            _ => None,
        })
        .collect();
    assert_eq!(published.len(), 2, "an unchanged display set is not resent");
    assert_eq!(published[0], vec![HudDisplay {
        uuid: LEFT.to_string(),
        frame: visible,
        backing_scale: 1.0,
    }]);
    assert_eq!(
        published[1].iter().map(|display| display.uuid.as_str()).collect::<Vec<_>>(),
        vec![LEFT, RIGHT]
    );
    assert_eq!(published[1][1].frame, right_screen());
}
