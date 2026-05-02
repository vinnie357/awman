//! `ReadyFrontend` impl for the CLI.
//!
//! Per WI 0069 §1, prompts on stdin for Dockerfile and legacy-migration
//! decisions when stdin is a TTY; otherwise returns the safe defaults
//! from §7u.

use crate::data::session::AgentName;
use crate::engine::container::frontend::ContainerFrontend;
use crate::engine::error::EngineError;
use crate::engine::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::engine::ready::{ReadyFrontend, ReadyPhase, ReadySummary};
use crate::engine::step_status::StepStatus;

use crate::frontend::cli::command_frontend::CliFrontend;

use super::helpers::yes_no;

impl ReadyFrontend for CliFrontend {
    fn ask_create_dockerfile(&mut self) -> Result<bool, EngineError> {
        Ok(yes_no("Dockerfile.dev not found; create one?", true))
    }

    fn ask_run_audit_on_template(&mut self) -> Result<bool, EngineError> {
        Ok(yes_no("run dockerfile audit on the template?", false))
    }

    fn ask_migrate_legacy_layout(&mut self, agent_name: &AgentName) -> Result<bool, EngineError> {
        Ok(yes_no(
            &format!("migrate legacy layout for agent {}?", agent_name.as_str()),
            false,
        ))
    }

    fn report_phase(&mut self, phase: &ReadyPhase) {
        self.messages.write_message(UserMessage {
            level: MessageLevel::Info,
            text: format!("ready phase: {phase:?}"),
        });
    }

    fn report_step_status(&mut self, step: &str, status: StepStatus) {
        self.messages.write_message(UserMessage {
            level: MessageLevel::Info,
            text: format!("ready step {step}: {status:?}"),
        });
    }

    fn container_frontend(&mut self) -> Box<dyn ContainerFrontend> {
        Box::new(super::container_frontend_marker::CliContainerProxy)
    }

    fn report_summary(&mut self, _summary: &ReadySummary) {}
}
