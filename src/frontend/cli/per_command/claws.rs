//! `ClawsFrontend` impl for the CLI.

use std::path::Path;

use crate::engine::claws::{ClawsFrontend, ClawsPhase, ClawsSummary};
use crate::engine::container::frontend::ContainerFrontend;
use crate::engine::error::EngineError;
use crate::engine::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::engine::step_status::StepStatus;

use crate::frontend::cli::command_frontend::CliFrontend;

use super::helpers::yes_no;

impl ClawsFrontend for CliFrontend {
    fn ask_replace_existing_clone(&mut self, path: &Path) -> Result<bool, EngineError> {
        Ok(yes_no(
            &format!("nanoclaw clone exists at {}; replace?", path.display()),
            false,
        ))
    }

    fn ask_run_audit(&mut self) -> Result<bool, EngineError> {
        Ok(yes_no("run claws audit?", false))
    }

    fn report_phase(&mut self, phase: &ClawsPhase) {
        self.messages.write_message(UserMessage {
            level: MessageLevel::Info,
            text: format!("claws phase: {phase:?}"),
        });
    }

    fn report_step_status(&mut self, step: &str, status: StepStatus) {
        self.messages.write_message(UserMessage {
            level: MessageLevel::Info,
            text: format!("claws step {step}: {status:?}"),
        });
    }

    fn container_frontend(&mut self) -> Box<dyn ContainerFrontend> {
        Box::new(super::container_frontend_marker::CliContainerProxy)
    }

    fn report_summary(&mut self, _summary: &ClawsSummary) {}
}
