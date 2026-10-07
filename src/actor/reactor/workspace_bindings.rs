//! Workspace display bindings.
//!
//! `workspace_rules` may bind a workspace to a display. Every native space still
//! owns the full workspace list, so a binding is enforced by routing rather than
//! by a different topology. While the owning display is connected, a bound
//! workspace is only ever shown there:
//!
//! - Each display starts on a workspace bound to it, or an unbound one.
//! - Cycling, relative window moves and back-and-forth on a display skip the
//!   workspaces bound to other displays.
//! - `switch_to_workspace`, or picking the workspace in the overview, focuses
//!   the owning display and switches there; `move_window_to_workspace` sends
//!   the window to the owning display's copy of the workspace, and
//!   `move_workspace_to_display` will not take a bound workspace off its display.
//! - Windows that end up in a bound workspace on another display (an app rule,
//!   an overview drop, or macOS parking them while the owner was unplugged) move
//!   to the owner, and after a display change a display showing a workspace
//!   bound elsewhere switches back to one of its own.
//!
//! When the owning display is not connected the workspace behaves like an
//! unbound one on whatever display the user is on.
//!
//! With `scope = "global"` every workspace has an owner, derived rather than
//! configured: the display whose copy of it is showing or holds windows (a
//! binding above forces the owner). A workspace no display owns does not exist
//! yet, as in i3, and opens on the display the user is on. The routing above
//! then applies to every workspace, which gives one shared set of workspaces
//! over the per-display copies: cycling on a display runs over its own and the
//! unowned workspaces, and back-and-forth has one target for all displays.

use tracing::warn;

use super::{DisplaySelector, EventOutcome, Reactor, ScreenInfo};
use crate::actor::app::WindowId;
use crate::actor::reactor::events::command as command_workflow;
use crate::common::collections::HashSet;
use crate::layout_engine::LayoutCommand;
use crate::model::VirtualWorkspaceId;
use crate::sys::screen::SpaceId;

/// Order screens by physical arrangement: left to right, then top to bottom.
///
/// `DisplaySelector::Index` counts displays in this order.
pub(crate) fn physical_order(screens: &[ScreenInfo]) -> Vec<&ScreenInfo> {
    let mut ordered: Vec<&ScreenInfo> = screens.iter().collect();
    ordered.sort_by(|a, b| {
        a.frame
            .origin
            .x
            .total_cmp(&b.frame.origin.x)
            .then_with(|| a.frame.origin.y.total_cmp(&b.frame.origin.y))
    });
    ordered
}

/// The screen among `screens` that a workspace binding names.
pub(crate) fn bound_screen<'a>(
    screens: &'a [ScreenInfo],
    binding: &DisplaySelector,
) -> Option<&'a ScreenInfo> {
    match binding {
        DisplaySelector::Uuid(uuid) => screens.iter().find(|screen| screen.display_uuid == *uuid),
        DisplaySelector::Index(index) => physical_order(screens).get(*index).copied(),
        DisplaySelector::Direction(_) => None,
    }
}

fn same_screen(a: &ScreenInfo, b: &ScreenInfo) -> bool {
    a.id == b.id && a.display_uuid == b.display_uuid
}

impl Reactor {
    pub(crate) fn has_workspace_display_bindings(&self) -> bool {
        self.config.virtual_workspaces.has_display_bindings()
    }

    /// Whether all displays share one set of workspaces.
    pub(crate) fn has_global_workspaces(&self) -> bool {
        self.config.virtual_workspaces.is_global()
    }

    /// Whether workspace commands may have to run on another display.
    fn routes_workspace_commands(&self) -> bool {
        self.config.virtual_workspaces.routes_workspaces()
    }

    /// Tell the workspace manager, for every display in `screens`, which
    /// workspace it starts on and which workspaces belong to another one.
    ///
    /// Runs on each forwarded snapshot before any new native space gets its
    /// workspaces, so windows found on a display land in one of its own, and on
    /// config reload. With global workspaces owners change with every switch
    /// and window move, so it also runs after each event and before a
    /// workspace command is routed.
    pub(crate) fn refresh_display_bindings(&mut self, screens: &[ScreenInfo]) {
        let bindings = self.display_bindings_for(screens);
        self.layout_manager
            .layout_engine
            .workspaces_mut()
            .set_display_bindings(bindings);
    }

    /// `refresh_display_bindings` for the displays currently on screen.
    pub(crate) fn refresh_workspace_owners(&mut self) {
        let bindings = self.display_bindings_for(&self.space_state.screens);
        self.layout_manager
            .layout_engine
            .workspaces_mut()
            .set_display_bindings(bindings);
    }

    /// Per display in `screens`: its native space, the workspace it starts on
    /// and the workspaces owned by another display.
    fn display_bindings_for(
        &self,
        screens: &[ScreenInfo],
    ) -> Vec<(SpaceId, Option<usize>, HashSet<usize>)> {
        if !self.routes_workspace_commands() {
            return Vec::new();
        }
        if !self.has_global_workspaces() {
            return self.configured_display_bindings_for(screens);
        }
        let settings = &self.config.virtual_workspaces;
        let count = settings.default_workspace_count.max(1);
        let default = settings.default_workspace;
        let workspaces = self.layout_manager.layout_engine.workspaces();
        let mut owners: Vec<Option<SpaceId>> =
            (0..count).map(|index| self.owner_space_among(screens, index)).collect();
        let mut bindings = Vec::with_capacity(screens.len());
        for screen in physical_order(screens) {
            let Some(space) = screen.space else {
                continue;
            };
            let owned_here = |owner: &Option<SpaceId>| *owner == Some(space);
            let foreign: HashSet<usize> = (0..count)
                .filter(|index| owners[*index].is_some_and(|owner| owner != space))
                .collect();
            // default_workspace if it may live here, else the first workspace
            // owned here, else the first unowned one.
            let start = (default < count && !foreign.contains(&default))
                .then_some(default)
                .or_else(|| owners.iter().position(owned_here))
                .or_else(|| owners.iter().position(Option::is_none));
            // A display without workspaces yet will own the one it starts on;
            // displays to its right start elsewhere.
            if workspaces.workspace_ids(space).is_empty()
                && let Some(start) = start
                && owners[start].is_none()
            {
                owners[start] = Some(space);
            }
            bindings.push((space, start, foreign));
        }
        bindings
    }

    /// `display_bindings_for` with per-display workspaces: only `workspace_rules`
    /// bind workspaces to displays.
    fn configured_display_bindings_for(
        &self,
        screens: &[ScreenInfo],
    ) -> Vec<(SpaceId, Option<usize>, HashSet<usize>)> {
        let settings = &self.config.virtual_workspaces;
        let count = settings.default_workspace_count.max(1);
        let default = settings.default_workspace;
        // Per workspace: `None` when unbound, else its owner if connected.
        let owners: Vec<Option<Option<&ScreenInfo>>> = (0..count)
            .map(|index| {
                settings
                    .display_binding_for_workspace(index)
                    .map(|binding| bound_screen(screens, binding))
            })
            .collect();
        screens
            .iter()
            .filter_map(|screen| {
                let owned_here = |owner: &Option<Option<&ScreenInfo>>| {
                    matches!(owner, Some(Some(owner)) if same_screen(owner, screen))
                };
                let foreign: HashSet<usize> = (0..count)
                    .filter(|index| {
                        matches!(owners[*index], Some(Some(_))) && !owned_here(&owners[*index])
                    })
                    .collect();
                // default_workspace if it may live here, else the first workspace
                // bound here, else the first unbound one, else one not foreign.
                let start = (default < count && !foreign.contains(&default))
                    .then_some(default)
                    .or_else(|| owners.iter().position(owned_here))
                    .or_else(|| owners.iter().position(Option::is_none))
                    .or_else(|| (0..count).find(|index| !foreign.contains(index)));
                Some((screen.space?, start, foreign))
            })
            .collect()
    }

    /// The active native space of the display workspace `index` is bound to, if
    /// that display is connected.
    fn bound_space_for_workspace_index(&self, index: usize) -> Option<SpaceId> {
        self.bound_space_among(&self.space_state.screens, index)
            .filter(|space| self.is_space_active(*space))
    }

    /// The native space of the display among `screens` workspace `index` is
    /// bound to.
    fn bound_space_among(&self, screens: &[ScreenInfo], index: usize) -> Option<SpaceId> {
        let binding = self.config.virtual_workspaces.display_binding_for_workspace(index)?;
        bound_screen(screens, binding)?.space
    }

    /// The active native space of the display workspace `index` lives on: the
    /// one it is bound to, or with global workspaces the one showing it or
    /// holding its windows. `None` means the workspace is unowned: bound to a
    /// disconnected display, or not existing yet.
    pub(crate) fn owner_space_for_workspace_index(&self, index: usize) -> Option<SpaceId> {
        self.owner_space_among(&self.space_state.screens, index)
            .filter(|space| self.is_space_active(*space))
    }

    /// `owner_space_for_workspace_index` among `screens`, which may be the
    /// displays of a snapshot not yet applied.
    fn owner_space_among(&self, screens: &[ScreenInfo], index: usize) -> Option<SpaceId> {
        self.bound_space_among(screens, index).or_else(|| {
            self.has_global_workspaces()
                .then(|| self.derived_owner_among(screens, index))
                .flatten()
        })
    }

    /// The display whose copy of workspace `index` is showing, else the one
    /// whose copy holds the most windows. Ties go to the leftmost display.
    fn derived_owner_among(&self, screens: &[ScreenInfo], index: usize) -> Option<SpaceId> {
        let workspaces = self.layout_manager.layout_engine.workspaces();
        let mut fullest: Option<(usize, SpaceId)> = None;
        for screen in physical_order(screens) {
            let Some(space) = screen.space else {
                continue;
            };
            let Some(copy) = workspaces.workspace_ids(space).get(index).copied() else {
                continue;
            };
            if workspaces.active_workspace(space) == Some(copy) {
                return Some(space);
            }
            let windows = self.state.windows.workspace_window_count(space, copy);
            if windows > 0 && fullest.is_none_or(|(most, _)| windows > most) {
                fullest = Some((windows, space));
            }
        }
        fullest.map(|(_, space)| space)
    }

    /// With global workspaces, remember which workspace has focus so that
    /// back-and-forth can return to the one focused before it, on any display.
    pub(crate) fn note_global_workspace_focus(&mut self) {
        if !self.has_global_workspaces() {
            return;
        }
        let Some(space) = self.command_context_space() else {
            return;
        };
        let workspaces = self.layout_manager.layout_engine.workspaces_mut();
        if let Some(index) = workspaces.active_workspace_idx(space) {
            workspaces.note_focused_workspace(index as usize);
        }
    }

    /// Bring the derived workspace owners and the focused workspace up to date
    /// after an event changed workspaces or windows.
    pub(crate) fn settle_global_workspaces(&mut self) {
        if !self.has_global_workspaces() {
            return;
        }
        self.refresh_workspace_owners();
        self.note_global_workspace_focus();
    }

    /// The workspace, by position, that `space` currently shows.
    fn active_workspace_index(&self, space: SpaceId) -> Option<usize> {
        self.layout_manager
            .layout_engine
            .workspaces()
            .active_workspace_idx(space)
            .map(|index| index as usize)
    }

    fn workspace_ordinal(&self, space: SpaceId, workspace: VirtualWorkspaceId) -> Option<usize> {
        let workspaces = self.layout_manager.layout_engine.workspaces();
        workspaces.workspace_ids(space).iter().position(|id| *id == workspace)
    }

    /// Redirect commands that show, or move a window to, a workspace owned by
    /// another display. `None` means the command takes the regular path.
    pub(crate) fn route_bound_workspace_command(
        &mut self,
        command: &LayoutCommand,
    ) -> Option<anyhow::Result<EventOutcome>> {
        if !self.routes_workspace_commands() {
            return None;
        }
        self.settle_global_workspaces();
        match command {
            LayoutCommand::SwitchToWorkspace(index) => {
                let here = self.command_context_space();
                if self.has_global_workspaces()
                    && here.is_some_and(|space| self.active_workspace_index(space) == Some(*index))
                {
                    // Already showing here. The workspace store's own
                    // back-and-forth is per display; use the shared target.
                    let last = self
                        .config
                        .virtual_workspaces
                        .workspace_auto_back_and_forth
                        .then(|| self.global_back_and_forth_target())
                        .flatten()
                        .filter(|last| last != index);
                    return Some(match last {
                        Some(last) => self.switch_to_workspace_anywhere(last),
                        None => Ok(EventOutcome::no_change()),
                    });
                }
                let owner = self.owner_space_for_workspace_index(*index)?;
                (here != Some(owner)).then(|| self.switch_to_bound_workspace(owner, *index))
            }
            LayoutCommand::SwitchToLastWorkspace if self.has_global_workspaces() => {
                let here = self.command_context_space();
                let last = self.global_back_and_forth_target().filter(|last| {
                    here.and_then(|space| self.active_workspace_index(space)) != Some(*last)
                });
                Some(match last {
                    Some(last) => self.switch_to_workspace_anywhere(last),
                    None => Ok(EventOutcome::no_change()),
                })
            }
            LayoutCommand::MoveWindowToWorkspace { workspace, follow, window_id } => {
                let window = self.resolve_command_window(*window_id)?;
                let source = self
                    .assigned_space_for_window_id(window)
                    .filter(|space| self.is_space_active(*space))?;
                let workspaces = self.layout_manager.layout_engine.workspaces();
                let target = workspaces.resolve_workspace(source, workspace)?;
                let index = self.workspace_ordinal(source, target)?;
                // An unowned workspace opens where the user is, which is not
                // necessarily the display the window is on.
                let owner = self.owner_space_for_workspace_index(index).or_else(|| {
                    self.has_global_workspaces().then(|| self.command_context_space()).flatten()
                });
                let owner = owner.filter(|owner| *owner != source)?;
                Some(self.move_window_to_bound_workspace(window, source, owner, index, *follow))
            }
            _ => None,
        }
    }

    /// The shared back-and-forth target, if it is a workspace that exists.
    fn global_back_and_forth_target(&self) -> Option<usize> {
        let count = self.config.virtual_workspaces.default_workspace_count.max(1);
        self.layout_manager
            .layout_engine
            .workspaces()
            .global_last_workspace()
            .filter(|index| *index < count)
    }

    /// Show workspace `index` on its owner, or on the display the user is on.
    fn switch_to_workspace_anywhere(&mut self, index: usize) -> anyhow::Result<EventOutcome> {
        let here = self.command_context_space();
        match self.owner_space_for_workspace_index(index).or(here) {
            Some(owner) if here != Some(owner) => self.switch_to_bound_workspace(owner, index),
            Some(space) => {
                self.dispatch_layout_command_on(LayoutCommand::SwitchToWorkspace(index), space)
            }
            None => Ok(EventOutcome::no_change()),
        }
    }

    /// Picking a display's copy of a workspace owned elsewhere in the overview
    /// switches to it on its owner instead.
    pub(crate) fn route_overview_workspace_selection(
        &mut self,
        space: SpaceId,
        index: usize,
    ) -> Option<anyhow::Result<EventOutcome>> {
        self.settle_global_workspaces();
        let owner = self.owner_space_for_workspace_index(index).filter(|owner| *owner != space)?;
        Some(self.switch_to_bound_workspace(owner, index))
    }

    /// Whether moving the workspace `source` shows to `target` would take a bound
    /// workspace off its display.
    pub(crate) fn bound_workspace_blocks_display_move(
        &self,
        source: SpaceId,
        target: SpaceId,
    ) -> bool {
        let active = self.layout_manager.layout_engine.workspaces().active_workspace(source);
        active
            .and_then(|active| self.workspace_ordinal(source, active))
            .and_then(|index| self.bound_space_for_workspace_index(index))
            .is_some_and(|owner| owner != target)
    }

    /// Resolve the window a workspace command acts on. A window named by id may
    /// live on any display, not just the one holding the command context.
    fn resolve_command_window(&self, window_id: Option<u32>) -> Option<WindowId> {
        let Some(idx) = window_id else {
            return self.layout_manager.layout_engine.focused_window();
        };
        let workspaces = self.layout_manager.layout_engine.workspaces();
        self.command_context_space()
            .into_iter()
            .chain(self.iter_active_spaces())
            .find_map(|space| workspaces.find_window_by_idx(&self.state.windows, space, idx))
    }

    /// Run a layout command through the regular command path on `space`.
    fn dispatch_layout_command_on(
        &mut self,
        command: LayoutCommand,
        space: SpaceId,
    ) -> anyhow::Result<EventOutcome> {
        let post_arrange_mouse_warp =
            self.config.settings.mouse_follows_focus.then(|| self.main_window()).flatten();
        let (visible_spaces, visible_space_frames) = self.visible_spaces_for_layout(false);
        command_workflow::handle_command_layout(
            &mut self.state,
            &mut self.layout_manager,
            &mut self.workspace_switch_manager,
            command_workflow::LayoutCommandPayload {
                command,
                command_space: Some(space),
                visible_spaces,
                visible_space_frames,
                post_arrange_mouse_warp,
            },
        )
    }

    /// Make `screen` the command and menu-bar context, as `focus_display` does.
    fn adopt_display_context(&mut self, screen: &ScreenInfo) {
        if crate::sys::screen::set_active_menu_bar_display_uuid(&screen.display_uuid) {
            self.space_state.menu_bar_space = screen.space;
        }
        self.space_state.command_space = screen.space;
    }

    fn switch_to_bound_workspace(
        &mut self,
        owner: SpaceId,
        index: usize,
    ) -> anyhow::Result<EventOutcome> {
        let Some(screen) = self.space_state.screen_by_space(owner).cloned() else {
            return Ok(EventOutcome::no_change());
        };
        let workspaces = self.layout_manager.layout_engine.workspaces_mut();
        if workspaces.workspace_id_at(owner, Some(index)) == workspaces.active_workspace(owner) {
            // Already showing there: go there, as focus_display does.
            return self.focus_display_by_selector(&DisplaySelector::Uuid(screen.display_uuid));
        }
        self.adopt_display_context(&screen);
        let outcome =
            self.dispatch_layout_command_on(LayoutCommand::SwitchToWorkspace(index), owner)?;
        // An empty workspace has nothing to focus; warp the cursor so the user
        // still lands on the display they asked for.
        let focuses = outcome
            .layout_responses
            .iter()
            .any(|(response, _)| response.focus_window.is_some());
        Ok(if focuses {
            outcome
        } else {
            outcome.with_mouse_warp(screen.frame.mid())
        })
    }

    fn move_window_to_bound_workspace(
        &mut self,
        window: WindowId,
        source: SpaceId,
        owner: SpaceId,
        index: usize,
        follow: bool,
    ) -> anyhow::Result<EventOutcome> {
        let Some(screen) = self.space_state.screen_by_space(owner).cloned() else {
            return Ok(EventOutcome::no_change());
        };
        let Some(state) = self.state.windows.window(window).filter(|_| !self.is_in_drag()) else {
            return Ok(EventOutcome::no_change());
        };
        let (window_server_id, frame) = (state.info.sys_id, state.frame_monotonic);
        let target_workspace = self
            .layout_manager
            .layout_engine
            .workspaces_mut()
            .workspace_id_at(owner, Some(index));
        let outcome = command_workflow::handle_command_reactor_move_window_to_display(
            &mut self.state,
            &mut self.layout_manager,
            command_workflow::MoveWindowToDisplayPayload {
                window,
                window_server_id,
                source_space: source,
                target_space: owner,
                target_screen: screen.frame,
                target_frame: Self::center_frame_on_screen(frame, screen.frame),
                target_workspace,
                follow,
            },
        )?;
        self.note_display_move_in_flight(window, owner);
        if follow {
            self.workspace_switch_manager
                .start_workspace_switch(super::WorkspaceSwitchOrigin::Manual);
            self.adopt_display_context(&screen);
        }
        Ok(outcome)
    }

    /// Whether a window sits in a workspace bound to the display now showing `space`.
    pub(crate) fn window_returns_to_bound_display(&self, wid: WindowId, space: SpaceId) -> bool {
        let Some(assignment) = self.state.windows.workspace_info_for_window(wid) else {
            return false;
        };
        self.workspace_ordinal(assignment.space, assignment.workspace_id)
            .and_then(|index| self.bound_space_for_workspace_index(index))
            == Some(space)
    }

    /// Re-apply bindings once the current event's outcome has settled.
    pub(crate) fn check_display_bindings_later(&mut self) {
        if self.has_workspace_display_bindings() {
            self.bindings_need_check = true;
        }
    }

    /// Re-apply bindings if an event asked for it: displays showing a workspace
    /// bound elsewhere switch back to one of their own, and windows in a bound
    /// workspace on another display move to its owner.
    pub(crate) fn apply_pending_display_bindings(&mut self) {
        // The moves emit layout events that ask for another check, which finds
        // everything in place. The bound guards against two displays handing a
        // window back and forth.
        for _ in 0..3 {
            if !self.bindings_need_check {
                return;
            }
            if self.refreshes_blocked() || self.is_in_drag() || self.is_mission_control_active() {
                // During sleep, wake, lock and display churn window and space data
                // is transient; moving windows on it fights macOS. The
                // authoritative snapshot that ends the instability runs this.
                return;
            }
            self.bindings_need_check = false;
            let mut outcome = EventOutcome::no_change();
            let normalized = self.normalize_bound_active_workspaces(&mut outcome);
            let rehomed = self.rehome_bound_windows(&mut outcome);
            if normalized || rehomed {
                self.apply_event_outcome(outcome);
            }
        }
    }

    /// Switch displays showing a workspace bound to another connected display
    /// back to their last own workspace, or the one they start on. When the owner
    /// shows nothing, it takes the workspace over so the user keeps seeing it.
    fn normalize_bound_active_workspaces(&mut self, outcome: &mut EventOutcome) -> bool {
        let mut changed = false;
        for screen in self.space_state.screens.clone() {
            let Some(space) = screen.space.filter(|space| self.is_space_active(*space)) else {
                continue;
            };
            let workspaces = self.layout_manager.layout_engine.workspaces();
            let Some(index) = workspaces
                .active_workspace(space)
                .and_then(|active| self.workspace_ordinal(space, active))
            else {
                continue;
            };
            let Some(owner) =
                self.bound_space_for_workspace_index(index).filter(|owner| *owner != space)
            else {
                continue;
            };
            let target = workspaces
                .last_workspace(space)
                .and_then(|last| self.workspace_ordinal(space, last))
                .unwrap_or_else(|| workspaces.starting_workspace(space));
            if target == index {
                continue;
            }
            let owner_workspaces = self.layout_manager.layout_engine.workspaces_mut();
            let owner_copy = owner_workspaces.workspace_id_at(owner, Some(index));
            let owner_active = owner_workspaces.active_workspace(owner);
            let owner_idle = owner_active.is_none_or(|id| {
                owner_workspaces.workspace_windows(&self.state.windows, owner, id).is_empty()
            });
            if owner_copy.is_some() && owner_active != owner_copy && owner_idle {
                match self
                    .dispatch_layout_command_on(LayoutCommand::SwitchToWorkspace(index), owner)
                {
                    Ok(switched) => outcome.absorb(switched),
                    Err(error) => warn!(%error, "failed to show a bound workspace on its display"),
                }
            }
            match self.dispatch_layout_command_on(LayoutCommand::SwitchToWorkspace(target), space) {
                Ok(switched) => {
                    outcome.absorb(switched);
                    changed = true;
                }
                Err(error) => warn!(%error, "failed to leave a workspace bound to another display"),
            }
        }
        changed
    }

    /// Move windows sitting in a bound workspace on another display to its owner.
    fn rehome_bound_windows(&mut self, outcome: &mut EventOutcome) -> bool {
        let workspaces = self.layout_manager.layout_engine.workspaces();
        let moves: Vec<_> = self
            .state
            .windows
            .iter_workspace_assignments()
            .filter(|(window, assignment)| {
                self.is_space_active(assignment.space)
                    // Visible where it is: a display only keeps showing a workspace
                    // bound elsewhere when it has none of its own.
                    && workspaces.active_workspace(assignment.space) != Some(assignment.workspace_id)
                    && self
                        .state
                        .windows
                        .window(*window)
                        .is_some_and(|state| state.is_admitted() && state.info.is_standard)
            })
            .filter_map(|(window, assignment)| {
                let index = self.workspace_ordinal(assignment.space, assignment.workspace_id)?;
                let owner = self
                    .bound_space_for_workspace_index(index)
                    .filter(|owner| *owner != assignment.space)?;
                Some((window, assignment.space, owner, index))
            })
            .collect();
        let mut changed = false;
        for (window, source, owner, index) in moves {
            match self.move_window_to_bound_workspace(window, source, owner, index, false) {
                Ok(moved) => {
                    changed |= moved.arrange.passes > 0;
                    outcome.absorb(moved);
                }
                Err(error) => {
                    warn!(?window, %error, "failed to move a window to its workspace's display")
                }
            }
        }
        changed
    }
}
