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
| # | Patch | Commits on `main` | Status |
|---|---|---|---|
| 1 | fix: raise storm (snapshot refocus) | `bcdf99b` | done; live since 2026-10-06 (old base, `d4d8b1b`) |
| 2 | fix: only refocus on the display that has focus | `88e097a` | done; live (old base) |
| 3 | fix: bound actor span chain (stack overflow) | `aed2ac2` | done; live (old base) |
| 4 | fix: reply to `save-and-exit` before exiting | `aac49fb` | done; live (old base) |
| 5 | PR #545: display-bound workspaces, multi-display fixes | `ddbba62` to `0cba564` (11) | done |
| 6 | i3-style global workspaces (`[virtual_workspaces] scope = "global"`) | `08c84dc` to `7cb53d5` (12) | done, off by default |
| 7 | aspect split (`auto_split_by_aspect`, `root_orientation`) | `09846e2`, `0d5abcf` | done, off by default |
| 8 | test isolation: unit tests never call into the live WindowServer | `fda497c` to `4436091` (5) | done (test-only) |
| 9 | layout persistence: `[settings.persistence]` autosave, `restore_on_start`, same-boot id matching | `c2db77f` to `75a2ce7` (5) | done, off by default |
| 10 | menu bar: dashed frame around a workspace with a fullscreen window | `e8b0b43`, `adbe92a` | done |
| 11 | native window borders + dimming (`[settings.ui.border]`, `[settings.ui.dim]`) | `b17bd6e` to `7171c2c` (6) | done, off by default |
| 12 | floating windows above tiled ones (`[settings] floating_windows_on_top`) | `ca789bf`, `d63800c` | done, off by default |
| 13 | input-freeze fix: event tap on its own thread, non-blocking timeout handler, tap breaker (`[settings] event_tap_timeout_limit`) | `b03e557`, `3bb738a` | done |

- Rows 1-4 are the former local branch `wms/fixes` (base 3a99afa, v0.6.7), rebased onto
  upstream 0.6.8. Root-cause write-ups: `wms/docs/incidents/2026-10-05-rift-*.md`.
- Row 5 is upstream PR #545 (@sj-cstar, open), picked with authorship kept; `9ff9f43` was
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
