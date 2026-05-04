//! `ExecWorkflowCommandFrontend` impl for the TUI.

use crate::command::commands::exec_workflow::{ExecWorkflowCommandFrontend, WorkflowSummary};
use crate::engine::message::UserMessageSink;
use crate::frontend::tui::command_frontend::TuiCommandFrontend;

impl ExecWorkflowCommandFrontend for TuiCommandFrontend {
    fn set_pty_active(&mut self, active: bool) {
        self.pty_active = active;
    }

    fn report_workflow_summary(&mut self, summary: &WorkflowSummary) {
        self.messages.info(format!(
            "Workflow: {} completed, {} failed",
            summary.steps_completed, summary.steps_failed
        ));
        if summary.steps_failed > 0 {
            self.messages
                .error_msg(format!("Failed steps: {}", summary.steps_failed));
        }
    }
}
