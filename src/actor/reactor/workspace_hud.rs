//! Feeding the workspace HUD actor.
//!
//! The HUD appears the moment a workspace switch is *decided*. Before the
//! reactor dispatches a user action that may switch workspaces (a layout
//! command that switches, an overview pick, an app activation that follows a
//! window to its workspace), [`Reactor::probe_workspace_hud`] notes the focused
//! display's workspace and the workspace each display shows. Right after the
//! dispatch, before the outcome's focus, raise and frame work runs,
//! [`Reactor::announce_workspace_switch`] compares and sends the HUD actor at
//! most one [`ShowRequest`]: a channel send, no WindowServer or AX query
//! (names, indices and display frames come from rift's own state).
//!
//! The comparison covers every switch kind: a switch on the focused display, a
//! global-scope switch routed to the display that owns the workspace (it may
//! only move focus there, when that display already shows it), back-and-forth,
//! next/prev, move-window with follow. Nothing else is probed, so space
//! snapshots and their `workspace_changed` echoes, startup restore, display
//! reconnect re-homing and the binding/settle passes that run after the
//! outcome never show the HUD.

use std::time::Instant;

use super::{Event, Reactor};
use crate::actor::app::Quiet;
use crate::actor::workspace_hud::{Event as HudEvent, ShowRequest};
use crate::layout_engine::LayoutCommand;
use crate::model::VirtualWorkspaceId;
use crate::model::reactor::Command;
use crate::model::workspace_hud::{HudDisplay, render_text};
use crate::sys::screen::SpaceId;

/// What kind of action may switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HudTrigger {
    /// A command naming a workspace (or the next, previous or last one): moving
    /// focus to the display that shows it counts as the switch.
    Command,
    /// An app activation: only a display that changed workspace counts.
    Activation,
}

/// What was showing before a probed action.
#[derive(Debug)]
pub(super) struct HudProbe {
    trigger: HudTrigger,
    /// The focused display's space and its workspace.
    focus: Option<(SpaceId, Option<VirtualWorkspaceId>)>,
    /// Each active display's space and its workspace.
    shown: Vec<(SpaceId, Option<VirtualWorkspaceId>)>,
    /// The display that had focus.
    focused_display: Option<String>,
}

impl Reactor {
    fn hud_trigger(event: &Event) -> Option<HudTrigger> {
        match event {
            Event::Command(Command::Layout(
                LayoutCommand::SwitchToWorkspace(_)
                | LayoutCommand::NextWorkspace(_)
                | LayoutCommand::PrevWorkspace(_)
                | LayoutCommand::SwitchToLastWorkspace
                | LayoutCommand::MoveWindowToWorkspace { follow: true, .. },
            ))
            | Event::OverviewSelectWorkspace { .. } => Some(HudTrigger::Command),
            Event::ApplicationActivated(_, Quiet::No) => Some(HudTrigger::Activation),
            _ => None,
        }
    }

    fn hud_active_workspace(&self, space: SpaceId) -> Option<VirtualWorkspaceId> {
        self.layout_manager.layout_engine.workspaces().active_workspace(space)
    }

    fn hud_focus(&self) -> Option<(SpaceId, Option<VirtualWorkspaceId>)> {
        self.workspace_command_space()
            .map(|space| (space, self.hud_active_workspace(space)))
    }

    fn hud_shown_spaces(&self) -> impl Iterator<Item = SpaceId> + '_ {
        self.space_state
            .screens
            .iter()
            .filter_map(|screen| screen.space)
            .filter(|space| self.is_space_active(*space))
    }

    /// Note what is showing before `event`, if the HUD is on and `event` is a
    /// user action that may switch workspaces.
    pub(super) fn probe_workspace_hud(&self, event: &Event) -> Option<HudProbe> {
        if !self.config.settings.ui.workspace_hud.enabled
            || self.communication_manager.workspace_hud_tx.is_none()
        {
            return None;
        }
        let trigger = Self::hud_trigger(event)?;
        let focus = self.hud_focus();
        Some(HudProbe {
            trigger,
            focus,
            shown: self
                .hud_shown_spaces()
                .map(|space| (space, self.hud_active_workspace(space)))
                .collect(),
            focused_display: focus.and_then(|(space, _)| self.display_uuid_for_space(space)),
        })
    }

    /// The display the probed action switched, if it switched one: the focused
    /// display when it shows another workspace than it did (or, for a
    /// command, focus moved to a display showing another workspace), else the
    /// first display that changed workspace.
    fn hud_switched_space(&self, probe: &HudProbe) -> Option<SpaceId> {
        let changed = |space: SpaceId| {
            let before = probe.shown.iter().find(|(shown, _)| *shown == space).map(|(_, ws)| *ws);
            before != Some(self.hud_active_workspace(space))
        };
        let focus = self.hud_focus();
        focus
            .filter(|(space, workspace)| {
                workspace.is_some()
                    && (changed(*space)
                        || (probe.trigger == HudTrigger::Command && focus != probe.focus))
            })
            .map(|(space, _)| space)
            .or_else(|| self.hud_shown_spaces().find(|space| changed(*space)))
    }

    /// Send the HUD actor the switch the probed action decided, if any.
    pub(super) fn announce_workspace_switch(&mut self, probe: HudProbe) {
        let Some(tx) = &self.communication_manager.workspace_hud_tx else {
            return;
        };
        let Some(space) = self.hud_switched_space(&probe) else {
            return;
        };
        let Some(screen) = self.space_state.screen_by_space(space) else {
            return;
        };
        if screen.display_uuid.is_empty() {
            return;
        }
        let engine = &self.layout_manager.layout_engine;
        let Some(workspace) = engine.workspaces().active_workspace(space) else {
            return;
        };
        let name = engine.workspace_name(space, workspace).unwrap_or_default();
        let index = engine
            .workspaces()
            .active_workspace_idx(space)
            .map_or(0, |index| index as usize + 1);
        let display_name = match &screen.name {
            Some(name) => name.clone(),
            None => {
                let position = self
                    .screens_in_physical_order()
                    .iter()
                    .position(|other| other.display_uuid == screen.display_uuid)
                    .map_or(1, |position| position + 1);
                format!("Display {position}")
            }
        };
        let text = render_text(
            &self.config.settings.ui.workspace_hud.text,
            &name,
            index,
            &display_name,
        );
        tx.send(HudEvent::Show(ShowRequest {
            text,
            target: screen.display_uuid.clone(),
            focused: probe.focused_display,
            decided_at: Instant::now(),
        }));
    }

    /// Tell the HUD actor the displays (visible frames from the cached screen
    /// state) when they change. Sent whether or not the HUD is on, so enabling
    /// it builds the overlays at once.
    pub(super) fn publish_hud_displays(&mut self) {
        let Some(tx) = &self.communication_manager.workspace_hud_tx else {
            return;
        };
        let screens =
            self.space_state.screens.iter().filter(|screen| !screen.display_uuid.is_empty());
        let unchanged = screens.clone().count() == self.hud_displays.len()
            && screens.clone().zip(&self.hud_displays).all(|(screen, display)| {
                screen.display_uuid == display.uuid
                    && screen.frame == display.frame
                    && screen.backing_scale == display.backing_scale
            });
        if unchanged {
            return;
        }
        let displays: Vec<HudDisplay> = screens
            .map(|screen| HudDisplay {
                uuid: screen.display_uuid.clone(),
                frame: screen.frame,
                backing_scale: screen.backing_scale,
            })
            .collect();
        self.hud_displays = displays.clone();
        tx.send(HudEvent::DisplaysChanged(displays));
    }
}
