//! `ImplementCommandFrontend` impl for the TUI.

use crate::command::commands::exec_workflow::WorkflowSummary;
use crate::command::commands::implement::ImplementCommandFrontend;
use crate::engine::message::UserMessageSink;
use crate::frontend::tui::command_frontend::TuiCommandFrontend;

impl ImplementCommandFrontend for TuiCommandFrontend {
    fn set_pty_active(&mut self, active: bool) {
        self.pty_active = active;
    }

    fn report_implement_summary(&mut self, summary: &WorkflowSummary) {
        self.messages.info(format!(
            "Implementation: {} completed, {} failed",
            summary.steps_completed, summary.steps_failed
        ));
    }
}
