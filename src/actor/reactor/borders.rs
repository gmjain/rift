//! Feeding the window border actor.
//!
//! After every event the reactor describes each managed display (its current
//! Space, frame, and the windows of the active workspace with their target
//! frames, focus, floating and fullscreen state) and sends the border actor the
//! displays whose description changed. Comparing against the last description
//! sent is what keeps focus changes to one message per affected display and
//! no-op snapshots silent. The dim overlay rides on the same description; a
//! WindowServer focus/raise event additionally resends the focused window's
//! display once so the dim overlay can re-order under it.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use super::Reactor;
use crate::actor::app::WindowId;
use crate::actor::border::{self, DisplaySnapshot};
use crate::common::collections::{HashMap, HashSet};
use crate::layout_engine::{FloatingFullscreenKind, LayoutSystem};
use crate::model::border::{BorderAnimation, BorderWindow, FullscreenKind};
use crate::sys::screen::SpaceId;

/// A focus report that changes nothing resends a display (to re-order its dim overlay) at most
/// this many times per [`FORCED_REORDER_WINDOW`]. Re-ordering touches only our own overlay
/// window; should WindowServer ever answer it with another focus report, this keeps the echo
/// to a few calls a second instead of a loop.
pub(super) const MAX_FORCED_REORDERS: usize = 4;
pub(super) const FORCED_REORDER_WINDOW: Duration = Duration::from_secs(1);

#[derive(Default)]
pub(super) struct BorderPublisher {
    /// What each display was last told, without the animation.
    last: HashMap<String, DisplaySnapshot>,
    /// Animations started by this event's layout passes, per space.
    motion: HashMap<SpaceId, BorderAnimation>,
    /// The window focus last sat on, per space, for `other_displays`.
    last_focused: HashMap<SpaceId, WindowId>,
    /// A focus/raise event asked for the focused display to be resent.
    reorder: bool,
    /// When each display was recently resent only to re-order (nothing else changed).
    pub(super) forced_at: HashMap<String, VecDeque<Instant>>,
}

impl BorderPublisher {
    /// Forget what was sent, so the next publish resends every display.
    pub(super) fn reset(&mut self) {
        self.last.clear();
        self.forced_at.clear();
    }

    /// The next publish resends the focused window's display even if unchanged.
    pub(super) fn request_reorder(&mut self) { self.reorder = true; }

    /// Record how a layout pass on `space` moves its windows.
    pub(super) fn note_motion(&mut self, space: SpaceId, motion: Option<BorderAnimation>) {
        match motion {
            Some(animation) => {
                self.motion.insert(space, animation);
            }
            None => {
                self.motion.remove(&space);
            }
        }
    }
}

impl Reactor {
    /// Send the border actor every display whose strokes may have changed.
    pub(super) fn publish_borders(&mut self) {
        let ui = &self.config.settings.ui;
        if !ui.border.enabled && !ui.dim.enabled {
            self.borders.reorder = false;
            return;
        }
        let Some(tx) = self.communication_manager.border_tx.clone() else {
            return;
        };
        let mut present: HashSet<String> = HashSet::default();
        let motion = std::mem::take(&mut self.borders.motion);
        let reorder = std::mem::take(&mut self.borders.reorder) && ui.dim.enabled;
        let now = Instant::now();
        for mut snapshot in self.border_snapshots() {
            present.insert(snapshot.display_uuid.clone());
            if let Some(focused) = snapshot.windows.iter().find(|window| window.focused) {
                self.borders.last_focused.insert(snapshot.space, focused.id);
            }
            snapshot.last_focused = self.borders.last_focused.get(&snapshot.space).copied();
            let focused_here = snapshot.windows.iter().any(|window| window.focused);
            let unchanged = self.borders.last.get(&snapshot.display_uuid) == Some(&snapshot);
            if unchanged {
                if !(reorder && focused_here) {
                    continue;
                }
                let recent =
                    self.borders.forced_at.entry(snapshot.display_uuid.clone()).or_default();
                recent.retain(|at| now.saturating_duration_since(*at) < FORCED_REORDER_WINDOW);
                if recent.len() >= MAX_FORCED_REORDERS {
                    continue;
                }
                recent.push_back(now);
            }
            self.borders.last.insert(snapshot.display_uuid.clone(), snapshot.clone());
            snapshot.animation = motion.get(&snapshot.space).copied();
            snapshot.reorder = reorder && focused_here;
            tx.send(border::Event::DisplayUpdated(snapshot));
        }
        let gone: Vec<String> = self
            .borders
            .last
            .keys()
            .filter(|uuid| !present.contains(*uuid))
            .cloned()
            .collect();
        for uuid in gone {
            self.borders.last.remove(&uuid);
            self.borders.forced_at.remove(&uuid);
            tx.send(border::Event::DisplayCleared(uuid));
        }
    }

    /// One description per display on an active, rift-managed Space.
    fn border_snapshots(&self) -> Vec<DisplaySnapshot> {
        let focused = self.main_window();
        let engine = &self.layout_manager.layout_engine;
        let mut snapshots = Vec::new();
        for screen in &self.space_state.screens {
            // A display without a Space here is on a native fullscreen Space or
            // not yet known; neither gets borders.
            let Some(space) = screen.space else { continue };
            if !self.is_space_active(space) || screen.display_uuid.is_empty() {
                continue;
            }
            let visible_tiled: HashSet<WindowId> = engine
                .workspaces()
                .active_layout_for_space(space)
                .map(|(workspace, layout)| {
                    engine.workspaces()[workspace]
                        .layout_system
                        .visible_windows_in_layout(layout)
                        .into_iter()
                        .collect()
                })
                .unwrap_or_default();
            let mut windows: Vec<BorderWindow> = engine
                .workspaces()
                .windows_in_active_workspace(&self.state.windows, space)
                .into_iter()
                .filter_map(|wid| {
                    let window = self.state.windows.window(wid)?;
                    // While rift animates a window its observed frame trails the
                    // target; the border goes where the window is going.
                    let frame = window
                        .info
                        .sys_id
                        .and_then(|wsid| self.transaction_manager.get_target_frame(wsid))
                        .unwrap_or(window.frame_monotonic);
                    let floating = engine.is_window_floating(wid);
                    let native_fullscreen = window.info.sys_id.is_some_and(|wsid| {
                        self.state.windows.is_window_server_id_native_fullscreen_suspended(wsid)
                    });
                    let visible = !window.info.is_minimized
                        && !native_fullscreen
                        && (floating || visible_tiled.contains(&wid));
                    Some(BorderWindow {
                        id: wid,
                        server_id: window.info.sys_id.map(|wsid| wsid.as_u32()),
                        frame,
                        focused: focused == Some(wid),
                        floating,
                        fullscreen: engine.fullscreen_kind(space, wid).map(|kind| match kind {
                            FloatingFullscreenKind::Full => FullscreenKind::Full,
                            FloatingFullscreenKind::WithinGaps => FullscreenKind::WithinGaps,
                        }),
                        visible,
                    })
                })
                .collect();
            windows.sort_by_key(|window| window.id);
            snapshots.push(DisplaySnapshot {
                display_uuid: screen.display_uuid.clone(),
                space,
                frame: screen.frame,
                backing_scale: screen.backing_scale,
                windows,
                last_focused: None,
                animation: None,
                reorder: false,
            });
        }
        snapshots
    }
}
