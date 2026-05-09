# new-amux issues

# Engines

ENG-1: An agent container being detected as stuck STILL does not trigger yolo countdown properly when running `exec workflow --yolo`. As soon as the container becomes stuck, it should trigger the yolo countdown. If `--yolo` was not passed, the WCB should be shown when the container gets detected as stuck. The frontend and workflow engine MUST collaborate to make this feature work properly, it is a key component of amux. Fix it. Currently, the yolo countdown ONLY runs after the current step's container exits. That is WRONG. It should start when the container becomes STUCK or when it EXITS. BOTH of those are valid reasons to start the yolo countdown.

**Status: FIXED**

Root cause: WI-0073 replaced the `ControlBoardRequest::StepStuck` channel send (which the engine handles: yolo countdown in yolo mode, WCB in non-yolo mode) with a direct TUI-side WCB dialog open. This completely bypassed the engine's yolo countdown logic — the engine never received `StepStuck` and never started a countdown.

Fix (`src/frontend/tui/app.rs`): Reverted the stuck detection block back to sending `ControlBoardRequest::StepStuck` via the engine's control board channel. The engine already has correct handling for it at `step_once_interruptible` lines 655–712: `StepStuck` → if yolo mode + auto enabled → `run_mid_step_yolo_countdown`; otherwise → `handle_mid_step_control_board` (WCB).

Additional fix: Added `if !tab.stuck { tab.yolo_dismissed_at = None; }` in `tick_all_tabs` so that when the container recovers (becomes un-stuck), the 60-second backoff resets. This enables the "yolo → Esc → container recovers → gets stuck again → yolo re-triggers" flow.

---

ENG-2: The init engine falsely claims there is an existing aspec/ folder when there is not. Also, aspec folder should only be a concern when `--aspec` is passed to `amux init`. Ensure all handling of the aspec folder and downloading the aspec template is handled correctly in `init`. Even when new-amux offers to set up the aspec folder, it creates the empty folder but does not download the actual template and place it in the directory.

**Status: FIXED**

Root cause 1 (false positive): `AwaitingAspecDecision` unconditionally called `frontend.ask_replace_aspec()`, which always printed "An aspec/ folder already exists" regardless of whether one was on disk. The frontend has no filesystem access, so the check was never done.

Root cause 2 (wrong scope): The `aspec/` setup should be skipped entirely unless `--aspec` is passed OR aspec/ already exists (replace-existing case).

Root cause 3 (download not happening): `CreatingAspecFolder` gated the template download on `self.options.run_aspec_setup` (the `--aspec` flag). When a user said "yes" to replacing an existing aspec/ without passing `--aspec`, `run_aspec_setup` was false and only an empty directory was created — no templates.

Fix (`src/engine/init/mod.rs`):
- `AwaitingAspecDecision`: Now checks `git_root.join("aspec").exists()` and `self.options.run_aspec_setup` before deciding. If `--aspec` → go directly to `CreatingAspecFolder`. If aspec/ exists → ask `ask_replace_aspec()`. If neither → skip (set Skipped, proceed to Dockerfile).
- `CreatingAspecFolder`: Removed the `run_aspec_setup` gate on the download. The phase is now only entered when we actually want the templates, so it always attempts to download and falls back to an empty directory only on network failure.
- Updated the `each_phase_independently_reachable_via_step` test to pre-create `aspec/` so the engine enters `AwaitingAspecDecision` → `ask_replace_aspec()` → `CreatingAspecFolder` path (matching the new logic).

---

# TUI

TUI-1: Pressing Ctrl-W ANY TIME a workflow is running should present the workflow control board. This is a UNIVERSAL RULE. If there is a workflow active in the current tab and the user presses Ctrl-W, show the board. no exceptions. Dismiss any other dialog and cancel the yolo timer if it's running. Ctrl-W must always be usable. When the user selects a valid option from the WCB, do that action! No exceptions! Any invalid option should be greyed out. Ctrl-Enter to end workflow should be greyed out unless it's the last step. If the user presses Esc to exit the WCB AND --yolo is true AND the agent container then becomes stuck, yolo countdown must be triggered again, even if the user previously dismissed the yolo countdown. `Stuck -> yolo -> Ctrl-W -> Opens WCB -> Esc -> Close WCB -> stuck -> yolo again` is a VALID FLOW and MUST WORK. `Ctrl-W while a container is running and action chosen` is VALID and MUST WORK. `Stuck -> yolo -> Esc -> Stuck again -> yolo again` is VALID and MUST WORK. `Stuck -> yolo -> Esc -> Ctrl-W -> Open WCB -> action chosen` MUST WORK.

**Status: FIXED**

Three specific issues were identified and fixed:

**1. Ctrl-W ignored when another dialog was open** (`src/frontend/tui/mod.rs`): The `Action::WorkflowControl` handler had `} else if app.active_dialog.is_some() { // Another dialog is blocking — don't interfere. }` which silently did nothing. Fixed: the blocking dialog is now dismissed via `dismiss_dialog` (which sends `Dismissed` to the command thread if needed), then `OpenControlBoard` is sent on the control board channel if a step is running. Ctrl-W now works regardless of what dialog is open.

**2. Yolo backoff not cleared after WCB Esc dismiss** (`src/frontend/tui/mod.rs`, `dismiss_dialog`): When the user opened the WCB (via Ctrl-W or stuck detection) and pressed Esc, the 60-second `yolo_dismissed_at` backoff was left active. This blocked yolo from re-triggering even if the container was still stuck. Fixed: `dismiss_dialog` now clears `yolo_dismissed_at` when the dismissed dialog is a `WorkflowControlBoard`. Enables the "Stuck → yolo → Ctrl-W → WCB → Esc → stuck → yolo again" flow.

**3. Yolo backoff not cleared after WCB action chosen** (`src/frontend/tui/mod.rs`, `handle_workflow_control_board_key`): When the user chose an action from the WCB (arrow key), `yolo_dismissed_at` was not cleared. If the chosen action didn't resolve the stuck state (e.g., "continue in same container"), yolo couldn't re-trigger. Fixed: `yolo_dismissed_at` is now set to `None` after any WCB navigation action.

The "Stuck → yolo → Esc → Ctrl-W → WCB → action" flow already worked: after Esc on the yolo countdown, `yolo_state` is cleared and no dialog is open, so the subsequent Ctrl-W hits the "no dialog, step running" branch which sends `OpenControlBoard`. No change needed for that path.

**4. Ctrl-W mid-step showed wrong dialog** (`src/frontend/tui/per_command/workflow_frontend.rs`): When the user pressed Ctrl-W while a container was actively running (mid-step), the engine received `OpenControlBoard` and called `user_choose_next_action` with `is_mid_step = true`. However, `user_choose_next_action` first checked `can_launch_next && !has_failures` — both true mid-step — and entered the lightweight `WorkflowStepConfirm` dialog path (showing "Step X completed, advance to Y?"), even though step X was still running. The user never saw the full WCB. Fixed: added `&& !available.is_mid_step` to the lightweight dialog condition so that `OpenControlBoard` mid-step always shows the full WCB.
