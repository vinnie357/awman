//! `ChatCommandFrontend` impl for the CLI.
//!
//! `ChatCommandFrontend` requires a `container_frontend()` accessor on
//! top of `UserMessageSink + MountScopeFrontend + AgentSetupFrontend +
//! AgentAuthFrontend`. The supertraits are already implemented on
//! `CliFrontend`; we only need to provide the accessor here.

use crate::command::commands::chat::ChatCommandFrontend;
use crate::engine::container::frontend::ContainerFrontend;

use crate::frontend::cli::command_frontend::CliFrontend;

impl ChatCommandFrontend for CliFrontend {
    fn container_frontend(&mut self) -> Box<dyn ContainerFrontend> {
        Box::new(super::container_frontend_marker::CliContainerProxy)
    }
}
