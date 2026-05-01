//! `ExecPromptCommand` — one-shot prompt injection.

use async_trait::async_trait;
use serde::Serialize;

use crate::command::commands::agent_auth::AgentAuthFrontend;
use crate::command::commands::agent_setup::AgentSetupFrontend;
use crate::command::commands::mount_scope::MountScopeFrontend;
use crate::command::commands::Command;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::engine::container::frontend::ContainerFrontend;
use crate::engine::message::UserMessageSink;

#[derive(Debug, Clone)]
pub struct ExecPromptCommandFlags {
    pub prompt: String,
    pub non_interactive: bool,
    pub plan: bool,
    pub allow_docker: bool,
    pub mount_ssh: bool,
    pub yolo: bool,
    pub auto: bool,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub overlay: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExecPromptOutcome {
    pub agent: Option<String>,
    pub exit_code: Option<i32>,
}

pub trait ExecPromptCommandFrontend:
    UserMessageSink + MountScopeFrontend + AgentSetupFrontend + AgentAuthFrontend + Send + Sync
{
    fn container_frontend(&mut self) -> Box<dyn ContainerFrontend>;
}

pub struct ExecPromptCommand {
    flags: ExecPromptCommandFlags,
    engines: Engines,
}

impl ExecPromptCommand {
    pub fn new(flags: ExecPromptCommandFlags, engines: Engines) -> Self {
        Self { flags, engines }
    }

    pub fn flags(&self) -> &ExecPromptCommandFlags {
        &self.flags
    }
}

#[async_trait]
impl Command for ExecPromptCommand {
    type Frontend = Box<dyn ExecPromptCommandFrontend>;
    type Outcome = ExecPromptOutcome;

    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        let _ = self.engines;
        frontend.replay_queued();
        Ok(ExecPromptOutcome {
            agent: self.flags.agent,
            exit_code: None,
        })
    }
}
