//! `WorkflowFrontend` impl for the CLI.
//!
//! Per WI 0069 §1, the CLI prompts on stdin (when it is a TTY) and falls
//! back to the safe non-interactive defaults from §7u otherwise. The
//! prompt presents only the actions in `AvailableActions` whose `can_*`
//! flags are true; excluded actions are skipped (with their
//! `*_unavailable_reason` printed as a parenthetical note).

use std::time::Duration;

use crate::data::workflow_definition::WorkflowStep;
use crate::data::workflow_state::WorkflowState;
use crate::engine::container::instance::ContainerExitInfo;
use crate::engine::error::EngineError;
use crate::engine::workflow::actions::{
    AvailableActions, NextAction, ResumeMismatch, StepFailureChoice, StepOutput,
    WorkflowOutcome, WorkflowStepStatus, YoloTickOutcome,
};
use crate::engine::workflow::frontend::WorkflowFrontend;

use crate::frontend::cli::command_frontend::CliFrontend;
use crate::frontend::cli::output::stdin_is_tty;

impl WorkflowFrontend for CliFrontend {
    fn user_choose_next_action(
        &mut self,
        _state: &WorkflowState,
        available: &AvailableActions,
    ) -> Result<NextAction, EngineError> {
        if !stdin_is_tty() {
            return Ok(NextAction::LaunchNext);
        }
        eprintln!("amux: workflow paused — choose next action:");
        if available.can_launch_next {
            eprintln!("  [n] Launch next step (new container)");
        }
        if available.can_continue_in_current_container {
            eprintln!("  [c] Continue in current container");
        } else if let Some(reason) = &available.continue_unavailable_reason {
            eprintln!("  (continue unavailable: {reason})");
        }
        if available.can_restart_current_step {
            eprintln!("  [r] Restart current step");
        }
        if available.can_cancel_to_previous_step {
            eprintln!("  [b] Back to previous step");
        } else if let Some(reason) = &available.cancel_to_previous_unavailable_reason {
            eprintln!("  (back unavailable: {reason})");
        }
        if available.can_pause {
            eprintln!("  [p] Pause workflow");
        }
        if available.can_abort {
            eprintln!("  [a] Abort workflow");
        }
        if available.can_finish_workflow {
            eprintln!("  [f] Finish workflow");
        } else if let Some(reason) = &available.finish_workflow_unavailable_reason {
            eprintln!("  (finish unavailable: {reason})");
        }
        let mut buf = String::new();
        if std::io::stdin().read_line(&mut buf).is_err() {
            return Ok(NextAction::Pause);
        }
        Ok(match buf.trim() {
            "n" | "N" if available.can_launch_next => NextAction::LaunchNext,
            "c" | "C" if available.can_continue_in_current_container => {
                NextAction::ContinueInCurrentContainer {
                    prompt: available.continue_prompt.clone().unwrap_or_default(),
                }
            }
            "r" | "R" if available.can_restart_current_step => NextAction::RestartCurrentStep,
            "b" | "B" if available.can_cancel_to_previous_step => {
                NextAction::CancelToPreviousStep
            }
            "p" | "P" if available.can_pause => NextAction::Pause,
            "a" | "A" if available.can_abort => NextAction::Abort,
            "f" | "F" if available.can_finish_workflow => NextAction::FinishWorkflow,
            _ => NextAction::Pause,
        })
    }

    fn confirm_resume(&mut self, _mismatch: &ResumeMismatch) -> Result<bool, EngineError> {
        if !stdin_is_tty() {
            return Ok(false);
        }
        eprintln!("amux: workflow file changed since last run; resume anyway? [y/n]");
        let mut buf = String::new();
        if std::io::stdin().read_line(&mut buf).is_err() {
            return Ok(false);
        }
        Ok(matches!(buf.trim(), "y" | "Y"))
    }

    fn user_choose_after_step_failure(
        &mut self,
        step: &WorkflowStep,
        exit: &ContainerExitInfo,
    ) -> Result<StepFailureChoice, EngineError> {
        if !stdin_is_tty() {
            return Ok(StepFailureChoice::Pause);
        }
        let signal_str = exit
            .signal
            .map(|s| s.to_string())
            .unwrap_or_else(|| "—".to_string());
        eprintln!(
            "amux: step '{}' failed (exit {}, signal {signal_str}). [r]etry / [p]ause / [a]bort?",
            step.name, exit.exit_code,
        );
        let mut buf = String::new();
        if std::io::stdin().read_line(&mut buf).is_err() {
            return Ok(StepFailureChoice::Pause);
        }
        Ok(match buf.trim() {
            "r" | "R" => StepFailureChoice::Retry,
            "a" | "A" => StepFailureChoice::Abort,
            _ => StepFailureChoice::Pause,
        })
    }

    fn report_step_status(&mut self, step: &WorkflowStep, status: WorkflowStepStatus) {
        let _ = (step, status);
    }

    fn report_step_output(&mut self, _step: &WorkflowStep, _output: StepOutput) {}

    fn report_step_stuck(&mut self, _step: &WorkflowStep) {}

    fn report_step_unstuck(&mut self, _step: &WorkflowStep) {}

    fn yolo_countdown_tick(
        &mut self,
        remaining: Duration,
    ) -> Result<YoloTickOutcome, EngineError> {
        use std::io::Write as _;

        if remaining.is_zero() {
            // Countdown expired: print a final message and a newline to move
            // off the countdown line before the engine prints the next step.
            eprintln!("\r  yolo: auto-advancing to next step...                            ");
            return Ok(YoloTickOutcome::Continue);
        }

        let secs = remaining.as_secs();
        eprint!(
            "\r  yolo: auto-advancing in {:2}s  [n] now  [a] abort  [p] pause    ",
            secs
        );
        let _ = std::io::stderr().flush();

        if !stdin_is_tty() {
            return Ok(YoloTickOutcome::Continue);
        }

        // Lazily spawn a background thread that reads stdin lines. The thread
        // runs for the lifetime of the countdown; when the Receiver is dropped
        // the next send will fail and the thread exits.
        if self.yolo_stdin_rx.is_none() {
            let (tx, rx) = std::sync::mpsc::channel::<String>();
            std::thread::spawn(move || {
                use std::io::BufRead as _;
                let stdin = std::io::stdin();
                for line in stdin.lock().lines() {
                    match line {
                        Ok(l) => {
                            if tx.send(l).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            });
            self.yolo_stdin_rx = Some(std::sync::Mutex::new(rx));
        }

        // Non-blocking check for a line the user already typed.
        if let Some(m) = &self.yolo_stdin_rx {
            if let Ok(rx) = m.try_lock() {
                match rx.try_recv() {
                    Ok(line) => {
                        return Ok(match line.trim() {
                            "n" | "N" => YoloTickOutcome::AdvanceNow,
                            "a" | "A" | "p" | "P" => YoloTickOutcome::Cancel,
                            _ => YoloTickOutcome::Continue,
                        });
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {}
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {}
                }
            }
        }

        Ok(YoloTickOutcome::Continue)
    }

    fn report_workflow_completed(&mut self, _outcome: &WorkflowOutcome) {}
}
