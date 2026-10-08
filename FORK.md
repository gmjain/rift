# gmjain/rift fork

Deliberate fork of [acsandmann/rift](https://github.com/acsandmann/rift) (Rust tiling WM for
macOS). Owner: Gaurav Jain. Canonical record of what diverges and why.
Workflow lives outside this repo: `wms/docs/rift-fork.md` (the `wms` workspace repo).

## Git flow
- `upstream` = acsandmann/rift, pull only (push URL `DISABLED`). Never push there.
- `origin` = gmjain/rift (this fork).
- `main` = linear patch queue: `upstream/main` + one commit per feature/fix. No merge commits.
- Features are built on `fork/<feature>` branches off `upstream/main`, verified, then
  integrated into `main` as a linear queue (same convention as the AeroSpace fork).
- Upstream sync: rebase `main` onto new `upstream/main`, drop commits upstream has absorbed.

## Patch list
Base: upstream 0.6.9 (`659eaa5`), rebased 2026-10-08; the 0.6.8-based queue is tag
`archive/main-pre-0.6.9-2026-10-08` (`1fe6c87`).

| # | Patch | Commits on `main` | Status |
|---|---|---|---|
| 1 | raise storm (snapshot refocus): regression tests; the fix is upstream #567 since 0.6.9 | `dd700b9` | slimmed to tests at the 0.6.9 rebase |
| 2 | fix: only refocus on the display that has focus | `ecc0b73` | done; live (0.6.8 build) |
| 3 | fix: bound actor span chain (stack overflow) | `990be3e` | done; live (0.6.8 build) |
| 4 | fix: reply to `save-and-exit` before exiting | `3e14523` | done; live (0.6.8 build) |
| 5 | PR #545: display-bound workspaces, multi-display fixes | `889603c` to `8c123ab` (11) | done |
| 6 | i3-style global workspaces (`[virtual_workspaces] scope = "global"`) | `1cfa7ce` to `fcec484` (12) | done, off by default |
| 7 | aspect split (`auto_split_by_aspect`, `root_orientation`) | `e208c9c`, `833a4f3` | done, off by default |
| 8 | test isolation: unit tests never call into the live WindowServer | `e42f42c` to `f099236` (5) | done (test-only) |
| 9 | layout persistence: `[settings.persistence]` autosave, `restore_on_start`, same-boot id matching | `8995938` to `d909fb3` (5) | done, off by default |
| 10 | menu bar: dashed frame around a workspace with a fullscreen window | `c2a89c8`, `e079b76` | done |
| 11 | native window borders + dimming (`[settings.ui.border]`, `[settings.ui.dim]`) | `6858889` to `0e4bf46` (6) | done, off by default |
| 12 | floating windows above tiled ones (`[settings] floating_windows_on_top`) | `1b06514`, `282a5ce` | done, off by default |
| 13 | input-freeze fix: event tap on its own thread, non-blocking timeout handler, tap breaker (`[settings] event_tap_timeout_limit`) | `80d8433`, `841fd70` | done |
| 14 | `workspace_changed` at once when a switch only focuses another display; no repeat for a snapshot that re-confirms it | `4793b63`, `ce59646` | done |
| 15 | built-in workspace HUD (`[settings.ui.workspace_hud]`), drawn at the switch decision | `043add6`, `a9ef97a` | done, off by default |
| 16 | a window in one bsp leaf on one display: duplicate-leaf fix for cross-display moves | `f6a1683` to `c90c308` (6) | done |
| 17 | HID event tap placement (`[settings] event_tap_placement`, `"head"`/`"tail"`): tail runs rift after Synergy's tap | `862d3ea`, `5778a2a` | done, head by default |

- Rows 1-4 are the former local branch `wms/fixes` (base 3a99afa, v0.6.7), rebased onto
  upstream 0.6.8, then 0.6.9. Root-cause write-ups: `wms/docs/incidents/2026-10-05-rift-*.md`.
  Row 1 was the fix (skip the refocus for a `WindowAdded` that re-confirms a parked window);
  upstream #567 (0.6.9) fixes the storm with a narrower rule (only a hidden window that holds
  focus asks for a replacement), which the fork's guard contradicted, so the guard was dropped
  and the two-display harness and its tests stay as regression cover for #567.
- Row 5 is upstream PR #545 (@sj-cstar, open), picked with authorship kept; `a2959e8` was
  reconciled with upstream #566 (both ordinal-keeping rules apply).
- Rows 6-7 change nothing until configured: `scope` defaults to `"per_display"`, the aspect keys
  to off. Design and review: `wms/docs/rift-global-workspaces.md`,
  `wms/docs/rift-fork-review-2026-10-06.md`.
- Row 8 is `cfg(test)`-only (production paths unchanged): fakes for window-server queries, no
  cursor/notification/event posting into the session, live SkyLight screen tests serialized.
  Four of the five commits apply upstream as they are.
- Row 9: autosave and `restore_on_start` default to off. Always on: layout files record the
  boot (`kern.bootsessionuuid`) and a file from this boot matches windows by process +
  WindowServer id alone; saves keep hidden floating frames; a saved window that shares a live
  window's id but does not match it no longer drops that window from the layout. Tooling:
  `wm restart-rift`, `wm to-rift --restore` (`wms/docs/switching.md`).
- Row 10 has no config key. Rows 11-12 change nothing until enabled; design and limits:
  `wms/docs/rift-borders.md`. Review of rows 8-12: `wms/docs/rift-fork-review-2026-10-06.md`
  (second pass).
- Row 13 fixes the 2026-10-06 input freeze (`wms/docs/incidents/2026-10-06-rift-input-freeze.md`):
  the HID tap runs on a user-interactive `event-tap` thread whose callback only reads shared
  in-memory state (1 ms lock wait, else pass-through, logged); cursor show/warp and stack-line
  occlusion move to the input actor; the timeout handler only re-enables the tap; after
  `event_tap_timeout_limit` (default 3) timeouts in 60 s input passes through unfiltered for
  5-60 s. Active by default (hotkey semantics unchanged); upstream-worthy. Review:
  `wms/docs/rift-fork-review-2026-10-06.md` (third pass).
- Row 14 has no config key. With global workspaces a switch to the workspace another display
  shows (and `focus_display`) reports `workspace_changed` with the command instead of ~200 ms
  later from the space snapshot; the engine remembers its last report (space, workspace, name)
  and a snapshot that only re-confirms it stays quiet. Workspace activations, focus moved by a
  click, native Space switches and renames still report. Upstream-worthy.
- Row 15 replaces the external HUD (macos-flash-centered-hud via a `workspace_changed`
  subscription): the reactor sends one show when it decides a user switch (before focus and
  raise work, no WindowServer/AX query), the main-thread actor draws a pre-built compositor
  overlay per display. Style presets card/pill/minimal/toast, 9 anchors, display choice.
  Off by default; the config block needs the new binary (`deny_unknown_fields`). Review of
  rows 14-15: `wms/docs/rift-fork-review-2026-10-06.md` (fourth pass).
- Row 16 fixes `wms/docs/incidents/2026-10-07-rift-duplicate-leaf.md`: bsp keeps one layout per
  display size and removed/renamed a window only in the indexed one, so a window moved to
  another display left a phantom leaf that came back (two leaves, two displays placing it). Now
  remove/replace walk every layout, insert is idempotent, a layout file is repaired on load,
  `WindowAdded` on another space moves the window, a layout pass never positions a window
  owned by another display, running presentations are fenced before global-scope moves, and a
  single (not animated) frame write the app answers with another frame is laid out once more.
  Upstream-worthy (issue draft in the incident doc). Review:
  `wms/docs/rift-fork-review-2026-10-06.md` (fifth pass).
- Row 17: with `"head"` (default, upstream) rift's tap runs before every other HID tap and
  consumes its hotkeys before Synergy can forward them to the other machine; restarting Synergy
  put it back in front only until rift's next tap rebuild. With `"tail"` the first tap and
  every rebuild (mask change, invalidation, failed re-enable, placement change on reload) use
  `kCGTailAppendEventTap`: HID taps that already exist (here Synergy, openlogi-agent, FineTune)
  and any tap another app later inserts at the head run first, and what they consume never
  reaches rift. Trade-offs: their remaps reach rift remapped; a key or click they swallow is
  invisible to rift (key/drag state stale until that key's or button's next event); Synergy's
  screen edges win over `horizontal_mouse_warp`. Nothing in rift needs to run first: apps, the
  Dock and session taps (Hammerspoon, Carbon hotkeys) follow every HID tap either way. Unit
  tests swap `EventTap` for a recorder (`cfg(test)`). Upstream-worthy.
- Status values: planned, in progress, done (on `main`), live (deployed), upstreamed, dropped.
- Update this table in the same commit that changes a patch's status.

## Build and deploy
- Built only through `wms/bin/build-rift` (shared build lock, signing, rollback dirs):
  `bin/build-rift --ref main`. Binaries land in `wms/dist/rift/`, not in this repo.
- Deploy: `wms/bin/wm to-rift` after a build. Never run two window managers at once.
- New config keys need the new binary first: rift uses `deny_unknown_fields`, so an unknown
  key crashes it at startup.

## Commit convention
- Rift style: `fix: ...`, `feat: ...`, lowercase, imperative, no trailing period.
- Agent-written commits end with trailer lines:
  `Co-Authored-By: <model> <noreply@anthropic.com>` and `Claude-Session: <url>`.
- Tests accompany every behaviour change (the raise-storm and span fixes ship regression tests).

## Upstream contributions
- Flow from this fork: a clean `fork/<feature>` branch rebased on `upstream/main` is the PR.
- Issues and PRs against acsandmann/rift need Gaurav's explicit go. Drafts only until then.
- Details: `wms/docs/rift-fork.md`.
