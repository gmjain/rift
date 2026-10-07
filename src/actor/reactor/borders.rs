//! Feeding the window border actor.
//!
//! After every event the reactor describes each managed display (its current
//! Space, frame, and the windows of the active workspace with their target
//! frames, focus, floating and fullscreen state) and sends the border actor the
//! displays whose description changed. Comparing against the last description
//! sent is what keeps focus changes to one message per affected display and
//! no-op snapshots silent.

use super::Reactor;
use crate::actor::app::WindowId;
use crate::actor::border::{self, DisplaySnapshot};
use crate::common::collections::{HashMap, HashSet};
use crate::layout_engine::{FloatingFullscreenKind, LayoutSystem};
use crate::model::border::{BorderAnimation, BorderWindow, FullscreenKind};
use crate::sys::screen::SpaceId;

#[derive(Default)]
pub(super) struct BorderPublisher {
    /// What each display was last told, without the animation.
    last: HashMap<String, DisplaySnapshot>,
    /// Animations started by this event's layout passes, per space.
    motion: HashMap<SpaceId, BorderAnimation>,
}

impl BorderPublisher {
    /// Forget what was sent, so the next publish resends every display.
    pub(super) fn reset(&mut self) { self.last.clear(); }

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
        if !self.config.settings.ui.border.enabled {
            return;
        }
        let Some(tx) = self.communication_manager.border_tx.clone() else {
            return;
        };
        let mut present: HashSet<String> = HashSet::default();
        let motion = std::mem::take(&mut self.borders.motion);
        for mut snapshot in self.border_snapshots() {
            present.insert(snapshot.display_uuid.clone());
            if self.borders.last.get(&snapshot.display_uuid) == Some(&snapshot) {
                continue;
            }
            self.borders.last.insert(snapshot.display_uuid.clone(), snapshot.clone());
            snapshot.animation = motion.get(&snapshot.space).copied();
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
                animation: None,
            });
        }
        snapshots
    }
}
