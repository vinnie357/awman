# Work Item: Feature

Title: restore mid-step workflow control dialog and mid-step actions
Issue: n/a — follow-up to WIs 0071 (TUI frontend) and 0073 (final parity)

## Prerequisites

- WI 0073 is complete: `oldsrc/` is gone, the four-layer architecture is the sole source of truth, the new test suite under `tests/` exists, and `make architecture-lint` is green.
- Familiarity with `aspec/architecture/2026-grand-architecture.md` (layer rules, frontend trait pattern).
- The TUI frontend's workflow plumbing as it stands after WI 0071 — specifically `src/frontend/tui/per_command/workflow_frontend.rs`, `src/frontend/tui/dialogs/mod.rs` (`WorkflowControlBoardState`, `WorkflowCancelConfirm`), the `[d]` toggle wired through `tab.workflow_state.auto_disabled`, and the `WorkflowFrontend` trait at `src/engine/workflow/frontend.rs`.

The implementing agent MUST read:

- `src/engine/workflow/mod.rs` (the engine driver loop).
- `src/engine/workflow/frontend.rs` and `src/engine/workflow/actions.rs` (the trait + decision types).
- `src/engine/workflow/factory.rs` (the `ContainerExecutionFactory` boundary).
- The TUI workflow plumbing under `src/frontend/tui/per_command/workflow_frontend.rs`, `src/frontend/tui/mod.rs` (key handling, `Action::CloseTabOrQuit` → `WorkflowCancelConfirm` branch), and `src/frontend/tui/keymap.rs`.

When uncertain about engine architecture trade-offs, ASK THE DEVELOPER rather than picking a half-baked path.

## Context

WI 0071 ported the TUI's workflow experience onto the new four-layer architecture. The engine drives the workflow loop; after every step completes it calls `WorkflowFrontend::user_choose_next_action(state, available)`, which in the TUI opens the `WorkflowControlBoard` dialog. That covers the **between-steps** path well.

What's missing is the **mid-step** path. In old amux the user could press `Ctrl+W` at any time during a workflow step to:

- Open the WorkflowControlBoard immediately (without waiting for the current step to finish).
- Pick `↑ Restart current step`, `← Cancel to previous step`, `→ Advance to next step (kill running container)`, or `Ctrl+Enter Finish workflow`.
- Have the running container killed and the engine re-driven from the chosen step.

In the new architecture the engine is mid-`step_once` waiting on the container's `wait()`; the user has no way to interrupt. They have only two escape valves today: `Esc` on the next-naturally-opened WCB (between-steps), or `Ctrl+C` → `WorkflowCancelConfirm` → full Abort. There is no equivalent of "kill this step and restart" or "kill this step and skip to the next" without aborting the entire workflow.

This work item restores that capability while respecting the new layering — the TUI must not call into runtime/git directly, and the engine must remain the only place that knows what the next step is.

## Summary

- Add an **interrupt channel** to `WorkflowEngine` so it can be told mid-step that the user wants to make a control-board decision now. The engine cancels the current step's container, recomputes `AvailableActions`, and calls `user_choose_next_action(state, available)` exactly as it would at a natural step boundary.
- Wire `Ctrl+W` in the TUI to send that interrupt request and open the WorkflowControlBoard with the engine's response.
- Extend `AvailableActions` (and the WCB renderer) so mid-step variants are distinguishable from between-step variants — primarily so the user understands "Restart current step" means "kill the running container and re-run from scratch", not just "rewind the bookkeeping".
- Track a per-tab `last_available_actions: Option<AvailableActions>` cache on the TUI side so the user can re-open the WCB from a previous decision point without round-tripping the engine when the engine is *not* mid-step (e.g. after the user dismissed the WCB with Esc and now wants it back).

## User Stories

### User Story 1:
As a: amux user running a long, multi-step workflow

I want to: press Ctrl+W mid-step and pick `Restart` when I notice the agent has gone in a wrong direction

So I can: re-run the current step without aborting the entire workflow and losing all of the prior steps' progress.

### User Story 2:
As a: amux user running a workflow

I want to: press Ctrl+W mid-step and pick `Cancel to previous step` when I realize an upstream step's output was wrong

So I can: re-do the upstream step (and re-derive everything that follows) without re-launching the workflow from the beginning.

### User Story 3:
As a: amux user running a workflow with a step that produced enough output that I'm satisfied

I want to: press Ctrl+W mid-step and pick `Advance to next step` (force-completing the current step)

So I can: skip ahead without waiting for the agent to terminate on its own.

### User Story 4:
As a: amux user who dismissed the WCB with Esc at a step boundary and changed my mind a second later

I want to: press Ctrl+W to re-open the WCB with the same action set the engine just offered me

So I can: reconsider without waiting for the next step to complete.

---

## Implementation Details

### 1. Engine interrupt channel

`WorkflowEngine` currently runs a synchronous-ish loop: `step_once()` spawns a container and `await`s `execution.wait()`. There is no path for an out-of-band signal to wake it up.

Add an interrupt receiver to the engine struct:

```rust
pub struct WorkflowEngine {
    // ...existing fields
    interrupt_rx: Option<tokio::sync::mpsc::UnboundedReceiver<InterruptRequest>>,
}

#[derive(Debug, Clone)]
pub enum InterruptRequest {
    /// User pressed Ctrl+W (or equivalent). Engine should kill the current
    /// step's container, compute `AvailableActions`, and call
    /// `user_choose_next_action`.
    OpenControlBoard,
}
```

Add a constructor variant `WorkflowEngine::with_interrupt(...)` that accepts the receiver. Update `WorkflowFrontend` (or the `WorkflowEngine::resume`/`run_to_completion` methods) so the frontend can take the *sender* end:

```rust
pub trait WorkflowFrontend: UserMessageSink + Send {
    // ...existing methods

    /// Optional: called once when the engine starts driving a workflow.
    /// Frontends that support mid-step interrupts (TUI) keep the sender; the
    /// CLI / headless frontends ignore it. Default impl is a no-op.
    fn set_interrupt_sender(
        &mut self,
        _tx: tokio::sync::mpsc::UnboundedSender<InterruptRequest>,
    ) {
    }
}
```

The engine's run loop becomes:

```rust
loop {
    let mut step_fut = Box::pin(self.step_once());
    tokio::select! {
        outcome = &mut step_fut => { /* normal completion path */ }
        Some(req) = recv_interrupt(self.interrupt_rx.as_mut()) => match req {
            InterruptRequest::OpenControlBoard => {
                // 1. Cancel the running container (best-effort).
                if let Some(exec) = self.current_execution.as_ref() {
                    let _ = exec.cancel();
                }
                // 2. Mark the running step as Pending in the persisted state
                //    (so a `Restart` re-runs it; an `Advance` marks it Done
                //    and continues; a `Cancel` rewinds the previous step).
                // 3. Compute `AvailableActions` for the mid-step case (see §3).
                // 4. Call `frontend.user_choose_next_action(state, available)`.
                // 5. Apply the user's choice and continue the outer loop.
            }
        }
    }
}
```

Document and unit-test the cancellation path: the container goes away, the writer / reader bridge tasks tear themselves down on PTY EOF, the next step starts in a fresh container.

### 2. TUI wiring

#### 2.1 Channels and state

Per-tab additions on `Tab`:

- `interrupt_tx: Option<tokio::sync::mpsc::UnboundedSender<InterruptRequest>>` — set when a workflow command spawns; consumed by `Ctrl+W` handler.
- `last_available_actions: Arc<Mutex<Option<AvailableActions>>>` — written by `workflow_frontend.rs::user_choose_next_action` so a Ctrl+W *between* engine prompts can re-open the WCB without round-tripping the engine.

`TuiCommandFrontend` gains a corresponding `last_available_actions: SharedAvailableActions` field, populated in `user_choose_next_action`.

The interrupt channel pair lives in `App::spawn_command` alongside the existing dialog/container channels. The sender goes to `Tab.interrupt_tx`; the receiver is bundled into the workflow frontend (or the `ContainerExecutionFactory`'s shared state) so the engine receives it via `set_interrupt_sender`.

#### 2.2 `Ctrl+W` keymap

Add `Action::OpenWorkflowControlBoard` to `keymap::Action`. Map it from `Ctrl+W` in:

- `FocusContext::CommandBox`
- `FocusContext::ExecutionWindow`
- `FocusContext::Dialog` is *not* re-bound (Ctrl+W must not interrupt other dialogs).

When the action fires:

1. Read `tab.workflow_state` — if no workflow is active, no-op (or push a status-bar hint "no workflow running").
2. If the engine is between steps (`tab.last_available_actions` is fresh AND no step is currently `Running`), open the WCB locally with the cached actions.
3. Otherwise, send `InterruptRequest::OpenControlBoard` over `tab.interrupt_tx`. The engine will respond by calling `user_choose_next_action` which opens the WCB through the normal dialog channel.

#### 2.3 Mid-step state on the dialog

Extend `WorkflowControlBoardState` with `is_mid_step: bool`. When true, the renderer:

- Title becomes `Workflow Control (mid-step)` so the user knows the running container will be killed by their choice.
- The `↑ Restart current step` and `→ Advance to next step` lines get a sub-bullet `↳ kills running container` in DarkGray.

### 3. Engine-side `AvailableActions` for the mid-step case

In the mid-step case the engine has just cancelled the current step's container. Recompute `AvailableActions` with these rules:

| Action | Mid-step availability |
|---|---|
| `LaunchNext` | Always available (kills current; treats current as Done, advances). |
| `ContinueInCurrentContainer` | **Never** — the container we'd reuse just got killed. Set `continue_unavailable_reason = "current step's container was cancelled"`. |
| `RestartCurrentStep` | Always available — re-runs current from Pending. |
| `CancelToPreviousStep` | Available iff a prior step exists. |
| `FinishWorkflow` | Available iff this is the last step. |
| `Pause` | Available — engine returns from `step_once`, caller decides what to do. |
| `Abort` | Always available. |

Do NOT pre-resolve a `continue_prompt` for mid-step calls (the channel is dead). The engine must guard `inject_prompt` against this case; today it would error out, which is acceptable.

### 4. Persistence rules

The current `WorkflowState` already tracks `Pending | Running { container_id } | Succeeded | Failed | Cancelled | Skipped`. Mid-step interrupts add nuance:

- On `OpenControlBoard` interrupt, the engine sets the current step's `StepState` to `Cancelled { reason: "user interrupt" }` *before* prompting. If the user picks `Restart`, the engine flips it back to `Pending` and re-runs.
- On `Advance`, the engine flips it to `Succeeded` (with a marker that distinguishes user-forced from naturally-completed; see "Edge cases" below). The persisted state should reflect this so a resume doesn't accidentally re-run a force-completed step.
- On `CancelToPrevious`, the engine flips both the current and the previous step back to `Pending`.
- On `FinishWorkflow`, every remaining step (current + downstream) goes to `Skipped`.

Add a new `StepCompletionMode { ContainerExited, UserForcedAdvance }` field on `Succeeded` *or* a separate `forced_succeeded_steps: HashSet<String>` on `WorkflowState` — whichever is less invasive. The schema version bumps; add a migration that defaults old-format steps to `ContainerExited`.

### 5. CLI and headless frontends

Default behaviour: ignore the interrupt channel. CLI users press `Ctrl+C` for the regular Abort path; headless clients have their own out-of-band control plane (a future work item can decide whether to expose this through the headless protocol).

`set_interrupt_sender` has a no-op default impl, so neither CLI nor headless frontends need updates.

---

## Edge Case Considerations

- **Race: container exits naturally just as the user presses Ctrl+W.** The engine receives both signals nearly simultaneously. Resolution: prefer the natural-completion path. The `tokio::select!` arm that wins is fine; if it's the interrupt arm, double-check `current_execution.cancel()` returns success — if the container is already gone, treat the interrupt as a regular between-steps WCB open.
- **Ctrl+W during a workflow that has no current Running step.** Likely the user is in the post-step pause, so just open the WCB locally with `last_available_actions`. If even that's empty (very early in the workflow), display a `Loading` dialog briefly while we send the interrupt and wait for the engine to respond.
- **Ctrl+W when `tab.interrupt_tx` is `None`.** The active tab isn't running a workflow — push a status-bar message ("no workflow running on this tab") and don't open a dialog.
- **Double-press: user presses Ctrl+W twice.** The second press while the WCB is open should be a no-op (Ctrl+W in `FocusContext::Dialog` is unbound).
- **Container cancel races with `try_inject_stdin`.** If the user picks `Restart` and a queued prompt was about to be injected from a previous between-steps decision, the new container won't see it. Engine must reset its "pending injection" buffer on `OpenControlBoard`.
- **Persisted state on crash mid-interrupt.** If amux crashes between cancelling the container and writing the new `Cancelled` state, the on-disk state still says `Running { container_id }`. On resume, `interrupted_running_steps()` already handles this — extend its handling so the user is prompted to Restart vs Skip.
- **The user picks `Pause` mid-step.** The engine returns from `step_once` with `Paused` — exactly the same path as a regular Pause. The TUI surfaces "Workflow paused — run again to resume" in the status log, the cancelled step stays in `Cancelled` state, and a resume re-runs it (this is consistent with old amux's "resume re-runs the last step that didn't complete").
- **Workflow strip update timing.** When the engine flips state mid-interrupt, it must also call `report_step_status` so the strip reflects the new state immediately (otherwise the user sees a stale `Running ●` after they picked `Restart` because the engine's normal status-update timing isn't tied to the interrupt path).
- **The `[d] disable auto-advance` toggle interaction.** A mid-step open is *always* user-initiated; the auto-advance gate doesn't apply. Document this in the WCB rendering — the `[d]` line can stay visible (toggling it for the current step is still useful for the engine's *next* between-steps decision).

## Test Considerations

### Engine tests (`src/engine/workflow/mod.rs#tests`)

- `interrupt_open_control_board_cancels_running_step_then_calls_user_choose_next_action`: drive the engine with a fake `ContainerExecutionFactory` whose first step "runs forever"; send `OpenControlBoard`; verify `cancel()` was called on the execution and `user_choose_next_action` was called with `is_mid_step` available actions.
- `mid_step_restart_re_runs_current_step_from_pending`: from the previous test, have the fake frontend pick `RestartCurrentStep`; verify the engine spawns a fresh container for the same step.
- `mid_step_advance_marks_step_force_succeeded_and_continues`: pick `LaunchNext` mid-step; verify the persisted state has the step as `Succeeded { force: true }` (or equivalent) and the next step starts.
- `mid_step_cancel_to_previous_rewinds_two_steps`: pick `CancelToPreviousStep`; verify both current and previous steps are `Pending` and the engine restarts from the previous one.
- `mid_step_pause_returns_paused_outcome_and_keeps_state_for_resume`: pick `Pause`; verify `run_to_completion` returns `WorkflowOutcome::Paused` and the persisted state keeps the `Cancelled` step. A subsequent resume re-runs that step.
- `interrupt_during_natural_completion_race_uses_natural_completion`: arrange both signals to fire together; verify the container's natural exit wins and the engine continues normally (no spurious cancel).
- `mid_step_continue_in_current_container_is_unavailable_with_reason`: verify the `continue_unavailable_reason` field is set in the mid-step `AvailableActions`.
- `resume_from_persisted_force_succeeded_step_does_not_re_run_it`: write a state file with a step marked force-succeeded, resume, verify it's skipped.

### TUI tests (`src/frontend/tui/per_command/workflow_frontend.rs#tests`)

- `user_choose_next_action_caches_available_actions`: verify `tab.last_available_actions` is populated after the engine opens the WCB.
- `ctrl_w_with_no_workflow_pushes_status_bar_message`: synthesize the keypress on a tab with no workflow; verify the status bar updates and no dialog opens.
- `ctrl_w_between_steps_uses_cached_available_actions`: with cached actions and no Running step, Ctrl+W should open the WCB locally without round-tripping the engine.
- `ctrl_w_during_running_step_sends_interrupt`: verify the interrupt sender on the tab receives `InterruptRequest::OpenControlBoard`.
- `mid_step_wcb_renders_kills_container_sub_bullet_for_advance_and_restart`: verify the rendered text contains the warning sub-bullets.
- `mid_step_wcb_continue_is_disabled_with_correct_reason`: verify the `[↓]` line is greyed out and the reason text appears.

### Integration tests (`tests/`)

- End-to-end: run a real two-step workflow with a slow first step, send Ctrl+W mid-step via crossterm event injection, pick Advance, verify the second step runs in a fresh container.
- End-to-end: same setup, pick Restart, verify two `docker run` invocations for the first step's container name.
- End-to-end: same setup, pick CancelToPrevious where possible (three-step workflow, interrupt the third), verify the second step re-runs.
- Persistence: kill amux mid-interrupt (between cancel-container and persist-state), restart, verify resume offers a sane recovery flow.

## Codebase Integration

- Follow the established conventions, best practices, testing, and architecture patterns from the project's `aspec/`. In particular:
  - Layer rules: the TUI must not call `runtime.stop_container` directly. The interrupt channel goes *into* the engine; the engine owns the cancel.
  - The `WorkflowFrontend` trait is the only TUI ↔ engine boundary for workflow concerns. Don't add ad-hoc shortcuts.
  - State persistence schema changes bump the version and ship a migration; do not break existing on-disk state files.
  - Keyboard bindings: register `Ctrl+W` in `keymap.rs` only — no scattered `KeyCode::Char('w')` matches in `mod.rs`.
- Update `docs/` to document the new mid-step Ctrl+W behaviour (it's the kind of thing power users will go looking for).
- Add tests at every layer: engine unit tests for the interrupt path, TUI tests for the keybinding and dialog state, end-to-end tests for the full user flow.
- The `make architecture-lint` target added in WI 0073 must continue to pass — no upward imports from engine into command/frontend.

---

## Also: Deferred Items From WIs 0071 and 0072

These were discovered during the WI 0071/0072 implementation passes (TUI + headless frontends) and explicitly deferred. They are bundled here so a single follow-up sprint closes the entire post-WI 0073 backlog — but the implementing agent MAY split them off into separate WIs (0075+) if scope grows.

Each item is small to medium; together they restore the last bits of old-amux UX parity that didn't make it into 0071/0072.

### A. Workflow → worktree integration

- **`ExecWorkflowCommand::run_with_frontend` never invokes the post-workflow worktree merge prompt.** Wire `WorktreeLifecycleFrontend::ask_post_workflow_action(branch, worktree_path)` after the engine returns `WorkflowOutcome::Completed`. On `Merge`, chain `ask_worktree_commit_before_merge` (when dirty), `confirm_squash_merge`, and `confirm_worktree_cleanup` exactly as `implement` does.
- **Cancel cleanup.** When the engine returns `Aborted` (user picked Abort, or Ctrl+C cancel), do not auto-discard the worktree — keep it on disk so the user can resume. Match old-amux semantics in `cancel_workflow_execution`.
- **Pre-commit warning dialog gets a rich rendering.** The current `worktree_lifecycle.rs` impl uses a generic `Custom` dialog. Add a dedicated `WorktreePreCommitWarningState` with `uncommitted_files: Vec<String>` and a renderer that shows up to 8 file paths (yellow) plus `… and N more` overflow. Mirror old amux's `draw_worktree_pre_commit_warning`.

### B. Yolo countdown overlay (non-modal rendering)

WI 0071 fixed the dialog-spam by writing to `SharedYoloState` instead of opening a fresh dialog every 100ms — but the renderer side never picks it up. Add to `render.rs`:

- A non-modal overlay strip rendered just above the status row when `tab.yolo_state.lock().unwrap().is_some()`. Format `Auto-advancing in {N}s · Esc to cancel · Press any key to dismiss`. Background magenta or yellow alternating each second to mirror old amux's tab-color animation.
- `Esc` while the overlay is visible clears `yolo_state` (which propagates `YoloTickOutcome::Cancel` to the engine on the next tick).
- For background tabs (not active): the alternating `⚠️ yolo in N` / `🤘 yolo in N` label and the yellow/magenta tab-color animation. Currently neither is implemented.

### C. Stuck-detection visual integration

`workflow_frontend.rs::report_step_stuck` and `report_step_unstuck` only emit status-log lines today. Wire them through:

- `report_step_stuck(step)` → set `view.steps[i].stuck = true` for the matching step. The strip renderer should add a yellow `⚠️` overlay glyph on the step's box border when stuck.
- `report_step_unstuck(step)` → clear it.
- Add a `stuck_steps: HashSet<String>` to `WorkflowViewState` if per-step granularity is preferred over mutating `steps[i]`.

### D. Engine consumption of `auto_disabled` set

WI 0071 wired `[d]` in the WCB to toggle `tab.workflow_state.auto_disabled` — but the engine never consults it. The engine should:

- Receive the auto-disabled set via the `WorkflowFrontend` trait (new method `should_auto_advance(step_name) -> bool`, default `true`).
- The TUI impl reads the set and returns `false` for disabled steps. The engine then skips the yolo-countdown auto-advance for that step and falls through to `user_choose_next_action`.

### E. Container summary stats history

`Tab::container_info.stats_history` is declared but never populated. The Docker / Apple `stats()` engine method exists; wire a periodic poller (every 5s) when a container is Maximized or Minimized that calls `runtime.stats(handle)` and pushes the result into `stats_history` and `latest_stats`. The `LastContainerSummary.avg_cpu/avg_memory` then become meaningful (currently always `n/a`).

### F. Command-box visual polish

- **Multi-line input rendering.** Today newlines display as `↵` in a single visible row and the cursor math ignores wrapping. Either grow the command box to accommodate multi-line input (`Constraint::Length(3 + extra_lines)` capped at, say, 8 rows) and render newlines as actual line breaks, OR clamp the input to one line and show a "(multi-line input — open editor with Ctrl+E)" hint.
- **Horizontal scroll for long input.** When `cursor_col` would push the cursor past the right border, scroll the visible portion of the input. Old amux just clipped, but a polished port would scroll.
- **`q to quit` hint when input is empty.** Old amux made `q` on an empty command box open `QuitConfirm` (already wired in WI 0071), but the *hint* was never visible. Add a DarkGray `(press q to quit)` ghost text when the input is empty and the command box is focused.

### G. Suggestions row enhancements

- **Worktree path display.** Today the fallback is always `  CWD: <path>`. When `tab.worktree_active_path` is set, show `  Using Worktree: <path>` instead (Blue label, DarkGray value), per old amux. Requires adding `worktree_active_path: Option<PathBuf>` to `Tab` and populating it from the `worktree` lifecycle.
- **Long path truncation.** When the path overflows the row, truncate with `…` from the *middle* (preserves both the host root prefix and the leaf directory).
- **Suggestion descriptors.** Old amux suggestions read `--flag <VAL>  —  hint` with the em-dash. The new catalogue-based suggestions only emit the flag name. Pull the flag's hint string from `CommandCatalogue` and render it after the em-dash.

### H. ConfigShow read-only field rejection

When the user presses Enter on a read-only row, the dialog silently no-ops. Add a transient toast / status-bar message `"This field is read-only"` so the user knows why nothing happened.

### I. PTY resize on container window cycle

`handle_resize` correctly forwards the new size to the container PTY — but cycling `Hidden → Maximized` (Ctrl+M) does not, even though the vt100 grid changes shape. Have `Action::CycleContainerWindow` re-send the current inner-area size through `tab.container_resize_tx` whenever the new state ≠ Hidden.

### J. `WorkflowStepConfirm` dialog (separate from WCB)

Old amux had a lightweight "advance to next step?" confirm dialog distinct from the full WCB, used in non-yolo, non-auto mode after each step. Currently the new TUI funnels everything through the heavier WCB. Restore the lightweight prompt:

- New `Dialog::WorkflowStepConfirm { completed_step, next_steps: Vec<String> }`.
- Triggered from `WorkflowFrontend::user_choose_next_action` when the engine could just as well show a one-liner ("Step `build` done. Advance to `test`? [Enter/y] yes / [q/n/Esc] pause"). The full WCB remains accessible via `Ctrl+W` (per the main item of this WI).

### K. Mouse-wheel scroll inside the workflow strip

Currently the workflow strip is read-only. When parallel groups overflow and `+ N more…` appears, allow scroll-wheel hover on the strip to cycle through the hidden steps. Low priority, but a polished touch.

### L. Documentation refresh

Update `docs/tui.md` (or whichever file documents TUI behaviour) to cover:
- All new keybindings introduced by WIs 0071/0072 and this WI.
- The Workflow Control Board's mid-step variants.
- The yolo countdown overlay and how to dismiss it.
- The auto-disable-for-step toggle and what it actually does.
- How to read the workflow strip (status glyphs, columns, parallel-group stacking).
