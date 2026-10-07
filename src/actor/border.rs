//! Window border actor: one click-through overlay per display, fed by the
//! reactor with the windows it manages there.
//!
//! The reactor sends a [`DisplaySnapshot`] only for displays whose windows,
//! focus or geometry changed; this actor turns each into strokes with the pure
//! model in [`crate::model::border`] and hands them to that display's overlay.

use objc2::MainThreadMarker;
use objc2_core_foundation::CGRect;
use tracing::{debug, instrument, warn};

use crate::actor;
use crate::actor::app::WindowId;
use crate::common::collections::HashMap;
use crate::common::config::Config;
use crate::model::border::{self, BorderAnimation, BorderWindow};
use crate::model::dim;
use crate::sys::screen::SpaceId;
use crate::ui::border::DisplayOverlay;

/// Everything the border model needs about one display.
#[derive(Debug, Clone, PartialEq)]
pub struct DisplaySnapshot {
    pub display_uuid: String,
    /// The display's current Space; the overlay lives on it.
    pub space: SpaceId,
    /// Display frame in global CG coordinates: the overlay window's frame.
    pub frame: CGRect,
    pub backing_scale: f64,
    /// Windows of the active workspace, sorted by id.
    pub windows: Vec<BorderWindow>,
    /// The window focus last sat on on this display (for dimming).
    pub last_focused: Option<WindowId>,
    /// How the windows move to these frames; `None` when they jump.
    pub animation: Option<BorderAnimation>,
    /// A focus/raise event: the dim overlay re-orders under the focused window.
    pub reorder: bool,
}

#[derive(Debug)]
pub enum Event {
    /// The display's strokes may have changed.
    DisplayUpdated(DisplaySnapshot),
    /// The display went away or shows a Space rift does not manage.
    DisplayCleared(String),
    ConfigUpdated(Config),
}

pub type Sender = actor::Sender<Event>;
pub type Receiver = actor::Receiver<Event>;

pub struct Border {
    config: Config,
    rx: Receiver,
    #[allow(dead_code)]
    mtm: MainThreadMarker,
    snapshots: HashMap<String, DisplaySnapshot>,
    overlays: HashMap<String, DisplayOverlay>,
}

impl Border {
    pub fn new(config: Config, rx: Receiver, mtm: MainThreadMarker) -> Self {
        Self {
            config,
            rx,
            mtm,
            snapshots: HashMap::default(),
            overlays: HashMap::default(),
        }
    }

    pub async fn run(mut self) {
        if !self.is_enabled() {
            debug!("window borders disabled at start; will listen for config changes");
        }
        while let Some((span, event)) = self.rx.recv().await {
            let _guard = span.enter();
            self.handle_event(event);
        }
    }

    fn is_enabled(&self) -> bool {
        self.config.settings.ui.border.enabled || self.config.settings.ui.dim.enabled
    }

    #[instrument(name = "border::handle_event", skip(self, event))]
    fn handle_event(&mut self, event: Event) {
        match event {
            Event::DisplayUpdated(snapshot) => {
                let uuid = snapshot.display_uuid.clone();
                let (animation, reorder) = (snapshot.animation, snapshot.reorder);
                self.snapshots.insert(uuid.clone(), snapshot);
                if self.is_enabled() {
                    self.render(&uuid, animation, reorder);
                }
            }
            Event::DisplayCleared(uuid) => {
                self.snapshots.remove(&uuid);
                self.overlays.remove(&uuid);
            }
            Event::ConfigUpdated(config) => self.handle_config_updated(config),
        }
    }

    fn handle_config_updated(&mut self, config: Config) {
        self.config = config;
        if !self.is_enabled() {
            // Dropping an overlay releases its WindowServer window; nothing is
            // left on screen and nothing is created until enabled again.
            self.overlays.clear();
            return;
        }
        let uuids: Vec<String> = self.snapshots.keys().cloned().collect();
        for uuid in uuids {
            self.render(&uuid, None, true);
        }
    }

    fn render(&mut self, uuid: &str, animation: Option<BorderAnimation>, reorder: bool) {
        let Some(snapshot) = self.snapshots.get(uuid) else {
            return;
        };
        let ui = &self.config.settings.ui;
        let borders = ui
            .border
            .enabled
            .then(|| border::compute(snapshot.frame, &snapshot.windows, &ui.border));
        let focused = snapshot.windows.iter().find(|window| window.focused).map(|window| window.id);
        let dim = dim::compute(
            snapshot.frame,
            &snapshot.windows,
            focused,
            snapshot.last_focused,
            if ui.border.enabled {
                ui.border.width
            } else {
                0.0
            },
            &ui.dim,
        );

        let reusable = self.overlays.get(uuid).is_some_and(|overlay| {
            overlay.matches(snapshot.space, snapshot.frame, snapshot.backing_scale)
        });
        if !reusable {
            // An overlay stays on the Space it was ordered in on, so a display
            // that moved to another Space (or changed geometry) gets a new one.
            self.overlays.remove(uuid);
            match DisplayOverlay::new(snapshot.space, snapshot.frame, snapshot.backing_scale) {
                Ok(overlay) => {
                    self.overlays.insert(uuid.to_string(), overlay);
                }
                Err(error) => {
                    warn!(?error, uuid, "failed to create window border overlay");
                    return;
                }
            }
        }
        let Some(overlay) = self.overlays.get_mut(uuid) else {
            return;
        };
        if let Err(error) = overlay.apply_borders(borders.as_ref(), animation) {
            warn!(?error, uuid, "failed to update window border overlay");
            self.overlays.remove(uuid);
            return;
        }
        if let Err(error) = overlay.apply_dim(dim, &ui.dim, reorder) {
            warn!(?error, uuid, "failed to update dim overlay");
            self.overlays.remove(uuid);
        }
    }
}

/// Snapshot helpers shared with the reactor's publisher.
impl DisplaySnapshot {
    pub fn window(&self, id: crate::actor::app::WindowId) -> Option<&BorderWindow> {
        self.windows.iter().find(|window| window.id == id)
    }
}
