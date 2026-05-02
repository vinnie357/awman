//! `InitFrontend` impl for the CLI.
//!
//! Per WI 0069 §1, the CLI prompts on stdin (when it is a TTY) for aspec
//! replacement, audit, and work-items config; otherwise it returns the
//! safe defaults from §7u.

use crate::data::config::repo::WorkItemsConfig;
use crate::engine::container::frontend::ContainerFrontend;
use crate::engine::error::EngineError;
use crate::engine::init::{InitFrontend, InitPhase, InitSummary};
use crate::engine::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::engine::step_status::StepStatus;

use crate::frontend::cli::command_frontend::CliFrontend;
use crate::frontend::cli::output::stdin_is_tty;

use super::helpers::yes_no;

impl InitFrontend for CliFrontend {
    fn ask_replace_aspec(&mut self) -> Result<bool, EngineError> {
        Ok(yes_no("aspec/ already exists; replace?", false))
    }

    fn ask_run_audit(&mut self) -> Result<bool, EngineError> {
        Ok(yes_no("run dockerfile audit now?", false))
    }

    fn ask_work_items_setup(&mut self) -> Result<Option<WorkItemsConfig>, EngineError> {
        if !stdin_is_tty() {
            return Ok(None);
        }
        eprintln!("amux: configure work-items directory? (empty = skip)");
        let mut buf = String::new();
        if std::io::stdin().read_line(&mut buf).is_err() {
            return Ok(None);
        }
        let dir = buf.trim();
        if dir.is_empty() {
            return Ok(None);
        }
        eprintln!("amux: work-items template path (empty = none)");
        let mut buf2 = String::new();
        let _ = std::io::stdin().read_line(&mut buf2);
        let template_str = buf2.trim();
        let template = if template_str.is_empty() {
            None
        } else {
            Some(template_str.to_string())
        };
        Ok(Some(WorkItemsConfig {
            dir: Some(dir.to_string()),
            template,
        }))
    }

    fn report_phase(&mut self, phase: &InitPhase) {
        self.messages.write_message(UserMessage {
            level: MessageLevel::Info,
            text: format!("init phase: {phase:?}"),
        });
    }

    fn report_step_status(&mut self, step: &str, status: StepStatus) {
        self.messages.write_message(UserMessage {
            level: MessageLevel::Info,
            text: format!("init step {step}: {status:?}"),
        });
    }

    fn container_frontend(&mut self) -> Box<dyn ContainerFrontend> {
        Box::new(super::container_frontend_marker::CliContainerProxy)
    }

    fn report_summary(&mut self, _summary: &InitSummary) {}
}
