//! What the reactor tells the window border actor, and when.
//!
//! The actor must hear about a display exactly when its strokes can change:
//! one message per affected display for a focus change, nothing for a
//! snapshot that changes nothing, a clear when a display goes away.

use objc2_core_foundation::{CGPoint, CGRect, CGSize};

use super::testing::*;
use super::*;
use crate::actor::app::pid_t;
use crate::actor::border::{DisplaySnapshot, Event as BorderEvent};
use crate::common::config::LayoutMode;
use crate::layout_engine::LayoutCommand;
use crate::model::border::{BorderAnimation, FullscreenKind};
use crate::sys::geometry::SameAs;

fn rect(x: f64, y: f64, w: f64, h: f64) -> CGRect {
    CGRect::new(CGPoint::new(x, y), CGSize::new(w, h))
}

fn border_context() -> (Apps, Reactor, actor::Receiver<BorderEvent>) {
    let (apps, mut reactor) = test_context();
    reactor.config.settings.ui.border.enabled = true;
    let (tx, rx) = actor::channel();
    reactor.communication_manager.border_tx = Some(tx);
    (apps, reactor, rx)
}

fn drain(rx: &mut actor::Receiver<BorderEvent>) -> Vec<BorderEvent> {
    let mut events = Vec::new();
    while let Ok((_, event)) = rx.try_recv() {
        events.push(event);
    }
    events
}

fn updates(events: Vec<BorderEvent>) -> Vec<DisplaySnapshot> {
    events
        .into_iter()
        .map(|event| match event {
            BorderEvent::DisplayUpdated(snapshot) => snapshot,
            other => panic!("expected a display update, got {other:?}"),
        })
        .collect()
}

fn focus(reactor: &mut Reactor, window: WindowId, space: SpaceId) {
    reactor.handle_event(Event::ApplicationGloballyActivated(window.pid));
    reactor.handle_event(Event::WindowServerFocusChanged(window, space));
    assert_eq!(reactor.main_window(), Some(window));
}

/// Two tiled windows of one app on one display.
fn one_display() -> (Apps, Reactor, actor::Receiver<BorderEvent>, SpaceId, CGRect) {
    let (mut apps, mut reactor, mut rx) = border_context();
    let screen = left_screen();
    let space = SpaceId::new(1);
    apps.make_app_and_settle_on_screen(&mut reactor, screen, space, 1, make_windows(2));
    let _ = drain(&mut rx);
    (apps, reactor, rx, space, screen)
}

/// One window per display, each in its display's active workspace.
fn two_displays() -> (Apps, Reactor, actor::Receiver<BorderEvent>, SpaceId, SpaceId) {
    let (mut apps, mut reactor, mut rx) = border_context();
    let (space1, space2) = (SpaceId::new(1), SpaceId::new(2));
    reactor.handle_event(space_state_event(vec![left_screen(), right_screen()], vec![
        Some(space1),
        Some(space2),
    ]));
    let pid: pid_t = 1;
    apps.make_app_and_settle(&mut reactor, pid, vec![
        make_window_info(rect(100., 100., 50., 50.), None, "left", None),
        make_window_info(rect(1100., 100., 50., 50.), None, "right", None),
    ]);
    assert_eq!(
        reactor.assigned_space_for_window_id(WindowId::new(pid, 1)),
        Some(space1)
    );
    assert_eq!(
        reactor.assigned_space_for_window_id(WindowId::new(pid, 2)),
        Some(space2)
    );
    let _ = drain(&mut rx);
    (apps, reactor, rx, space1, space2)
}

#[test]
fn layout_pass_describes_the_display_once() {
    let (mut apps, mut reactor, mut rx) = border_context();
    let screen = left_screen();
    let space = SpaceId::new(1);
    apps.make_app_and_settle_on_screen(&mut reactor, screen, space, 1, make_windows(2));
    let events = drain(&mut rx);
    let last = match events.last() {
        Some(BorderEvent::DisplayUpdated(snapshot)) => snapshot.clone(),
        other => panic!("expected a display update last, got {other:?}"),
    };
    assert!(events.iter().all(|event| matches!(event, BorderEvent::DisplayUpdated(_))));
    assert_eq!(last.display_uuid, "test-display-0");
    assert_eq!(last.space, space);
    assert_eq!(last.frame, screen);
    assert_eq!(last.backing_scale, 1.0);
    assert_eq!(last.windows.len(), 2);
    for window in &last.windows {
        let laid_out = reactor.state.windows.window(window.id).unwrap().frame_monotonic;
        assert!(window.frame.same_as(laid_out), "{window:?} vs {laid_out:?}");
        assert!(window.visible);
        assert!(!window.floating);
        assert_eq!(window.fullscreen, None);
        assert!(!window.focused, "nothing is focused yet");
    }
    assert!(last.windows.windows(2).all(|pair| pair[0].id < pair[1].id));
}

#[test]
fn a_snapshot_that_changes_nothing_is_silent() {
    let (mut apps, mut reactor, mut rx, space, screen) = one_display();
    reactor.handle_event(space_state_event(vec![screen], vec![Some(space)]));
    apps.simulate_until_quiet(&mut reactor);
    assert!(drain(&mut rx).is_empty());

    // Re-asserting the focused window changes nothing either.
    focus(&mut reactor, WindowId::new(1, 1), space);
    let _ = drain(&mut rx);
    reactor.handle_event(Event::WindowServerFocusChanged(WindowId::new(1, 1), space));
    assert!(drain(&mut rx).is_empty());
}

#[test]
fn focus_change_on_one_display_updates_it_exactly_once() {
    let (_apps, mut reactor, mut rx, space, _screen) = one_display();
    let (first, second) = (WindowId::new(1, 1), WindowId::new(1, 2));

    focus(&mut reactor, first, space);
    let snapshots = updates(drain(&mut rx));
    assert_eq!(snapshots.len(), 1, "{snapshots:?}");
    assert!(snapshots[0].window(first).unwrap().focused);
    assert!(!snapshots[0].window(second).unwrap().focused);

    reactor.handle_event(Event::WindowServerFocusChanged(second, space));
    let snapshots = updates(drain(&mut rx));
    assert_eq!(snapshots.len(), 1, "{snapshots:?}");
    assert_eq!(snapshots[0].display_uuid, "test-display-0");
    assert!(!snapshots[0].window(first).unwrap().focused);
    assert!(snapshots[0].window(second).unwrap().focused);
}

#[test]
fn focus_change_across_displays_updates_each_affected_display_once() {
    let (_apps, mut reactor, mut rx, space1, space2) = two_displays();
    let (left, right) = (WindowId::new(1, 1), WindowId::new(1, 2));

    focus(&mut reactor, left, space1);
    let snapshots = updates(drain(&mut rx));
    assert_eq!(
        snapshots.len(),
        1,
        "only the left display changed: {snapshots:?}"
    );
    assert_eq!(snapshots[0].display_uuid, "test-display-0");
    assert!(snapshots[0].window(left).unwrap().focused);

    reactor.handle_event(Event::WindowServerFocusChanged(right, space2));
    let mut snapshots = updates(drain(&mut rx));
    snapshots.sort_by(|a, b| a.display_uuid.cmp(&b.display_uuid));
    assert_eq!(snapshots.len(), 2, "{snapshots:?}");
    assert_eq!(snapshots[0].display_uuid, "test-display-0");
    assert_eq!(snapshots[0].space, space1);
    assert!(!snapshots[0].window(left).unwrap().focused);
    assert_eq!(snapshots[1].display_uuid, "test-display-1");
    assert_eq!(snapshots[1].space, space2);
    assert!(snapshots[1].window(right).unwrap().focused);
    assert!(
        snapshots[1].window(left).is_none(),
        "other displays' windows are not listed"
    );
}

#[test]
fn windows_in_inactive_workspaces_are_not_described() {
    let (mut apps, mut reactor, mut rx, space, _screen) = one_display();
    focus(&mut reactor, WindowId::new(1, 1), space);
    let _ = drain(&mut rx);

    reactor.handle_test_layout_command(LayoutCommand::NextWorkspace(None));
    apps.simulate_until_quiet(&mut reactor);
    let snapshots = updates(drain(&mut rx));
    assert!(!snapshots.is_empty());
    assert!(
        snapshots.last().unwrap().windows.is_empty(),
        "{:?}",
        snapshots.last()
    );

    reactor.handle_test_layout_command(LayoutCommand::PrevWorkspace(None));
    apps.simulate_until_quiet(&mut reactor);
    let snapshots = updates(drain(&mut rx));
    assert_eq!(snapshots.last().unwrap().windows.len(), 2);
}

#[test]
fn fullscreen_and_floating_state_are_reported() {
    let (mut apps, mut reactor, mut rx, space, screen) = one_display();
    let window = WindowId::new(1, 1);
    focus(&mut reactor, window, space);
    let _ = drain(&mut rx);

    reactor.handle_test_layout_command(LayoutCommand::ToggleFullscreen);
    apps.simulate_until_quiet(&mut reactor);
    let snapshots = updates(drain(&mut rx));
    let last = snapshots.last().expect("fullscreen changes the display");
    let described = last.window(window).unwrap();
    assert_eq!(described.fullscreen, Some(FullscreenKind::Full));
    assert!(described.frame.same_as(screen), "{described:?}");

    reactor.handle_test_layout_command(LayoutCommand::ToggleFullscreen);
    reactor.handle_test_layout_command(LayoutCommand::ToggleFullscreenWithinGaps);
    apps.simulate_until_quiet(&mut reactor);
    let snapshots = updates(drain(&mut rx));
    let described = snapshots.last().unwrap().window(window).unwrap();
    assert_eq!(described.fullscreen, Some(FullscreenKind::WithinGaps));

    reactor.handle_test_layout_command(LayoutCommand::ToggleFullscreenWithinGaps);
    reactor.handle_test_layout_command(LayoutCommand::ToggleWindowFloating);
    apps.simulate_until_quiet(&mut reactor);
    let snapshots = updates(drain(&mut rx));
    let described = snapshots.last().unwrap().window(window).unwrap();
    assert_eq!(described.fullscreen, None);
    assert!(described.floating);
    assert!(described.visible);
}

#[test]
fn config_reload_toggles_publishing_and_resends_every_display() {
    let (mut apps, mut reactor, mut rx) = border_context();
    reactor.config.settings.ui.border.enabled = false;
    let screen = left_screen();
    let space = SpaceId::new(1);
    apps.make_app_and_settle_on_screen(&mut reactor, screen, space, 1, make_windows(2));
    assert!(drain(&mut rx).is_empty(), "disabled borders publish nothing");

    let mut config = reactor.config.clone();
    config.settings.ui.border.enabled = true;
    reactor.handle_event(Event::ConfigUpdated(config));
    let events = drain(&mut rx);
    assert!(
        matches!(events.first(), Some(BorderEvent::ConfigUpdated(config)) if config.settings.ui.border.enabled),
        "{events:?}"
    );
    let snapshots = updates(events.into_iter().skip(1).collect());
    assert_eq!(snapshots.len(), 1, "{snapshots:?}");
    assert_eq!(snapshots[0].windows.len(), 2);

    // A width change alone resends the display so the actor can restroke it.
    let mut config = reactor.config.clone();
    config.settings.ui.border.width = 9.0;
    reactor.handle_event(Event::ConfigUpdated(config));
    let events = drain(&mut rx);
    assert!(matches!(events.first(), Some(BorderEvent::ConfigUpdated(_))));
    assert_eq!(updates(events.into_iter().skip(1).collect()).len(), 1);

    let mut config = reactor.config.clone();
    config.settings.ui.border.enabled = false;
    reactor.handle_event(Event::ConfigUpdated(config));
    let events = drain(&mut rx);
    assert_eq!(events.len(), 1, "{events:?}");
    assert!(matches!(events[0], BorderEvent::ConfigUpdated(_)));
    focus(&mut reactor, WindowId::new(1, 1), space);
    assert!(drain(&mut rx).is_empty());
}

#[test]
fn updates_carry_the_layout_pass_animation_and_focus_changes_none() {
    let (mut apps, mut reactor, mut rx, space, _screen) = one_display();
    assert!(!reactor.config.settings.animate, "test reactors do not animate");

    // Windows jump: so do their strokes.
    reactor.handle_test_layout_command(LayoutCommand::ToggleOrientation);
    apps.simulate_until_quiet(&mut reactor);
    let snapshots = updates(drain(&mut rx));
    assert!(!snapshots.is_empty());
    assert!(snapshots.iter().all(|snapshot| snapshot.animation.is_none()));

    // Focus alone moves no window: the restroke is instant.
    focus(&mut reactor, WindowId::new(1, 1), space);
    let _ = drain(&mut rx);
    reactor.handle_event(Event::WindowServerFocusChanged(WindowId::new(1, 2), space));
    let snapshots = updates(drain(&mut rx));
    assert_eq!(snapshots.len(), 1, "{snapshots:?}");
    assert_eq!(snapshots[0].animation, None);

    // A scrolling workspace animates regardless of the global switch (and of
    // low power mode), so the layout pass that re-tiles it carries the motion.
    reactor.config.settings.animation_duration = 0.2;
    reactor.handle_test_layout_command(LayoutCommand::SetWorkspaceLayout {
        workspace: None,
        mode: LayoutMode::Scrolling,
    });
    apps.simulate_until_quiet(&mut reactor);
    let snapshots = updates(drain(&mut rx));
    assert!(!snapshots.is_empty());
    for snapshot in &snapshots {
        assert_eq!(
            snapshot.animation,
            Some(BorderAnimation { duration: 0.2 }),
            "{snapshot:?}"
        );
    }
}

#[test]
fn a_display_that_goes_away_is_cleared() {
    let (mut apps, mut reactor, mut rx, space1, _space2) = two_displays();
    reactor.handle_event(space_state_event(vec![left_screen()], vec![Some(space1)]));
    apps.simulate_until_quiet(&mut reactor);
    let events = drain(&mut rx);
    let cleared: Vec<&String> = events
        .iter()
        .filter_map(|event| match event {
            BorderEvent::DisplayCleared(uuid) => Some(uuid),
            _ => None,
        })
        .collect();
    assert_eq!(cleared, vec!["test-display-1"], "{events:?}");

    // A display on a native fullscreen Space (no Space known to rift) is cleared too.
    reactor.handle_event(space_state_event(vec![left_screen()], vec![None]));
    apps.simulate_until_quiet(&mut reactor);
    let events = drain(&mut rx);
    assert!(
        matches!(events.as_slice(), [BorderEvent::DisplayCleared(uuid)] if uuid == "test-display-0"),
        "{events:?}"
    );
}
