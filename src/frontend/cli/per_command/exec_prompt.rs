//! `ExecPromptCommandFrontend` impl for the CLI.

use crate::command::commands::exec_prompt::ExecPromptCommandFrontend;
use crate::engine::container::frontend::ContainerFrontend;

use crate::frontend::cli::command_frontend::CliFrontend;

impl ExecPromptCommandFrontend for CliFrontend {
    fn container_frontend(&mut self) -> Box<dyn ContainerFrontend> {
        Box::new(super::container_frontend_marker::CliContainerProxy)
    }
}
