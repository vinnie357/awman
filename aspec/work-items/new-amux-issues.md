# New-amux issues

## TUI

TUI-1: The bottom text does not show 'Using worktree...' when `exec workflow` runs in a worktree, it shows the CWD. Fix it.

**Status:** Fixed.

**Fix:** Added a `SharedActiveWorktreePath` (`Arc<Mutex<Option<PathBuf>>>`) to
`Tab` and `TuiCommandFrontend`. The TUI worktree-lifecycle frontend now
publishes the worktree path on `report_worktree_created` *and* when the user
chooses Resume in `ask_existing_worktree` (the lifecycle returns early in
that case without re-emitting Created). It clears the path on
`report_worktree_discarded` / `report_worktree_kept`. The bottom-bar
`render_suggestion_row` reads the shared path and shows
`Using worktree: <path>` whenever it is set, before falling back to the
`working_dir != git_root` heuristic and finally to `CWD: <path>`.

---

TUI-2: When the yolo dialog is shown in a workflow, it says you can press Ctrl-W for the workflow control board, but pressing Ctrl-W just dismisses the yolo dialog and no workflow control dialog shows up.

**Status:** Fixed.

**Fix:** In the `Action::WorkflowControl` handler we used to clear the shared
`yolo_state` *before* setting the `yolo_ctrl_w` atomic. If the engine ticked
between those two writes it observed `yolo_state.is_none() && yolo_initialized
== true` and returned `YoloTickOutcome::Cancel` (treated as Pause) instead of
`ShowControlBoard`. The order is now reversed: set the atomic first, then
clear `yolo_state`. The engine's next tick sees the atomic, returns
`ShowControlBoard`, and falls through to `user_choose_next_action` which
sends the `WorkflowControlBoard` dialog request.

---

TUI-3: The container window PTY still doesn't let me scroll all the way to the bottom AND still limits 50 lines of scrollback even when there are 1000+ available. Fix scrolling properly to work like old-amux.

**Status:** Fixed.

**Fix:** Swapped the `vt100 = "0.15"` dependency for the maintained fork
`vt100-ctt = "0.17"` (aliased back to `vt100` in `Cargo.toml` so call
sites stay unchanged). vt100 0.15.2's `Grid::visible_rows()` does an
unchecked `rows_len - scrollback_offset` subtraction that panics in
builds with `overflow-checks=true` (debug / dev) the moment the offset
exceeds the screen height; vt100-ctt 0.17 patches both halves of the
chain — `take(rows_len)` upper-bounds the scrollback portion and
`rows_len.saturating_sub(self.scrollback_offset)` makes the live-rows
take safe. With that fix in place we now allow `container_scroll_offset`
to grow up to the full scrollback buffer depth (the per-repo
`terminal_scrollback_lines` config — 5000 by default) on scroll-up, and
saturating-sub on scroll-down brings the user back to live (offset 0).
A regression test in `tabs::tests::deep_scroll_past_screen_rows_does_not_panic`
seeds 500 lines of scrollback, sets the offset to the full depth, and
asserts that `Screen::cell` returns without panic — exactly the case
that crashed with vt100 0.15.

API adjustments for the fork:
- `Parser::set_size` / `Parser::set_scrollback` moved to `Screen` (now
  `parser.screen_mut().set_size(rows, cols)` / `…set_scrollback(n)`).
- `Cell::contents()` now returns `&str` (was `String`); call sites that
  needed an owned `String` add `.to_string()`.

---

TUI-4: After a workflow completes while running in a worktree, the optiones include 'merge into main branch' which may be misleading if the branch being merged into is not `main`. Change to `merge into current branch' or fetch the actual branch name if you can, for clarity

**Status:** Fixed (layered correctly: engine → command → frontend).

**Fix:**
- **Engine layer (`engine/git/mod.rs`):** added `GitEngine::current_branch`
  which runs `git rev-parse --abbrev-ref HEAD` and returns `None` for
  detached HEAD or git failures. This is the only place that talks to git.
- **Command layer (`command/commands/worktree_lifecycle.rs`):** introduced
  a `PostWorkflowWorktreePrompt` struct holding all dialog content
  (`title`, `body`, `merge_label`, `discard_label`, `keep_label`,
  resolved `target_branch`). `WorktreeLifecycle::finalize` calls
  `git_engine.current_branch` (falling back to the literal
  `"current branch"`), composes the prompt — including the user-facing
  string `Merge into '<actual-branch-name>'` — and passes it to the
  frontend.
- **Frontend layer:** `WorktreeLifecycleFrontend::ask_post_workflow_action`
  now takes `&PostWorkflowWorktreePrompt`. The TUI/CLI/headless impls
  render the strings as-is and never compose copy themselves; this keeps
  the prompt wording testable in one place and prevents divergence
  between frontends.

---

TUI-5: The 'Commit before merge?" dialog cuts off the text, shows no hints, and accepts no input except Esc. Fix it.

**Status:** Fixed.

**Fix:** The dialog used the fixed-size `YesNo` (50×8) which clipped a long
file list and the trailing `[y]/[n]` hint. Replaced it with a `Custom`
dialog (auto-sizing to body width and length, with explicit hint rows) and
made the keys explicit: `[y] Commit, then merge` /
`[n] Skip commit, merge as-is`. Audit-side: also upgraded `YesNo` itself to
auto-size and wrap so other call sites can't hit the same clipping bug.

---

TUI-6: The container stats shown in the top-right of the container window always shoe 0 CPU and 0 memory used. Figure out what's broken for Apple containers.

**Status:** Fixed.

**Fix:** The Apple stats path was looking for Docker-style fields
(`CPUPerc`, `MemUsage`) that the Apple `container` CLI doesn't emit; both
parses fell back to `0`. Apple's `container stats --no-stream --format json`
emits `cpuUsageUsec` (cumulative microseconds) and `memoryUsageBytes` (raw
bytes), so single-shot CPU% can't be derived. The new implementation in
`engine/container/apple.rs::stats` mirrors old-amux: take two samples ~200ms
apart, compute `cpu_delta_usec / elapsed_usec * 100` for CPU%, and convert
`memoryUsageBytes / 1MiB` for memory. Defensive JSON parsing handles both
array and per-line shapes.

## Engines

ENG-1: While `exec workflow` does detect if there is an active worktree and asks to resume using it or re-create it, it does not detect and existing workflow state file and ask if the workflow should be resumed or deleted and started fresh. Ensure it asks about workflow resumption AND worktree reuse/recreate when each thing is found on disk, respectively.

**Status:** Fixed.

**Fix:** `exec workflow` now checks for a persisted state file before PTY
activation (so the dialog renders cleanly, mirroring the existing-worktree
prompt). Added an `ask_workflow_resume_or_fresh(workflow_name,
completed_steps, total_steps) -> bool` method on
`ExecWorkflowCommandFrontend`; the TUI implementation opens a `Custom`
dialog showing progress and offering `[r] Resume from saved state` /
`[f] Delete state and start fresh`. CLI/headless default to resume. When
the user picks fresh, the state file is deleted; either way the engine
construction switched from `WorkflowEngine::new` to `WorkflowEngine::resume`
so the saved state is actually consulted (resume already handled hash drift
via `confirm_resume`; that path is unchanged).

---

ENG-2: When running a workflow with --yolo, the yolo dialog shows up in the TUI but never starts counting down; it sticks at 60 and nothing advances. Ensure the countdown and auto-advance work properly.

**Status:** Fixed.

**Fix:** Two yolo-countdown dialog drivers were active: the engine-driven
one (via the shared `yolo_state`, ticking down every 100ms) and a TUI-side
stuck-detection one in `tick_all_tabs` that set `tab.yolo_countdown =
Some(60)` and never decremented it. When stuck-detection fired during step
execution it opened a "yolo countdown" dialog stuck at 60 — exactly the
symptom reported. Removed the stuck-detection yolo branch entirely; stuck
detection now opens the workflow control board (the engine retains sole
ownership of inter-step countdowns, which advances correctly via
`yolo_state.remaining_secs`).

---

## Dialog audit (follow-up)

Audited every TUI dialog against the requested checklist:
1. **Adequate sizing** — converted fixed-size dialogs (`YesNo`,
   `YesNoCancel`, `WorkflowControlBoard`, `WorkflowStepError`,
   `WorkflowYoloCountdown`, `AgentSetup`, `MountScope`, `AgentAuth`,
   `Loading`, `WorkflowStepConfirm`, `Custom`, plus `QuitConfirm`,
   `CloseTabConfirm`, `WorkflowCancelConfirm`) to dynamic width/height
   based on content (display width via `unicode_width`, longest body line,
   key-label widths, title width). Heights grow to fit body lines + hints
   without clipping. `MultilineInput` is now 70%×60% of the area.
2. **All text rendered as intended** — added `Wrap { trim: false }` to
   dialogs whose body could overflow horizontally
   (`WorkflowStepError`, `MountScope`, `AgentAuth`, `WorkflowYoloCountdown`,
   `WorkflowStepConfirm`, `Custom`, etc.) so long lines wrap rather than
   being silently truncated. `ListPicker` now windows items so the selected
   row remains visible when the list is taller than the dialog.
3. **Cursor in text fields** — verified `TextInput` and `MultilineInput`
   place the cursor with `frame.set_cursor_position`. The `MultilineInput`
   layout was tightened to keep the cursor inside the content rect after
   reserving a hint row.
4. **Hints at bottom for keys** — every dialog now has an explicit hint
   row(s) listing keys: `TextInput` → `[Enter] submit / [Esc] cancel`;
   `MultilineInput` → `[Ctrl+Enter] submit / [Enter] newline / [Esc]
   cancel`; `ListPicker` → `[↑/↓] navigate / [Enter] select / [Esc]
   cancel`; `KindSelect` → `[1-9] select / [Esc] cancel`; etc.
5. **Padding** — `render_dialog_frame` already adds 1 cell horizontal +
   1 row vertical inner padding inside the rounded border. All dialogs go
   through that helper, so padding is uniform; sizing now budgets for it
   (no content presses against the border).
