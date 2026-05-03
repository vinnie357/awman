//! `ClawsFrontend` trait — defined by Layer 1, implemented by Layer 3.

use std::path::Path;

use crate::engine::claws::phase::ClawsPhase;
use crate::engine::claws::summary::ClawsSummary;
use crate::engine::container::frontend::ContainerFrontend;
use crate::engine::error::EngineError;
use crate::engine::message::UserMessageSink;
use crate::engine::step_status::StepStatus;

pub trait ClawsFrontend: UserMessageSink + Send {
    fn ask_replace_existing_clone(&mut self, path: &Path) -> Result<bool, EngineError>;
    fn ask_run_audit(&mut self) -> Result<bool, EngineError>;
    fn report_phase(&mut self, phase: &ClawsPhase);
    fn report_step_status(&mut self, step: &str, status: StepStatus);
    fn container_frontend(&mut self) -> Box<dyn ContainerFrontend>;
    fn report_summary(&mut self, summary: &ClawsSummary);

    /// `claws ready` found a stopped controller — should it be restarted?
    /// Default: yes (safe default for non-interactive sinks).
    fn confirm_restart_stopped(&mut self) -> Result<bool, EngineError> {
        Ok(true)
    }

    /// `claws ready` found no controller — should we offer to initialize one?
    /// Default: no (safe default for non-interactive sinks; the user must opt
    /// in to a multi-step init flow).
    fn confirm_offer_init(&mut self) -> Result<bool, EngineError> {
        Ok(false)
    }

    /// Approve a list of sudo commands the engine plans to execute as part of
    /// permission setup (writing to a system-owned clone dir, etc.). Returns
    /// `true` to proceed, `false` to skip the permission step (the engine then
    /// records `permissions_check = StepStatus::Skipped`).
    ///
    /// Default: `false` — non-interactive sinks must opt in explicitly.
    fn confirm_sudo_actions(&mut self, _commands: &[String]) -> Result<bool, EngineError> {
        Ok(false)
    }
}
