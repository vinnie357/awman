//! `ChatCommand` — freeform chat with the configured agent.

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
pub struct ChatCommandFlags {
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
pub struct ChatOutcome {
    pub agent: Option<String>,
    pub exit_code: Option<i32>,
}

pub trait ChatCommandFrontend:
    UserMessageSink + MountScopeFrontend + AgentSetupFrontend + AgentAuthFrontend + Send + Sync
{
    fn container_frontend(&mut self) -> Box<dyn ContainerFrontend>;
}

pub struct ChatCommand {
    flags: ChatCommandFlags,
    engines: Engines,
}

impl ChatCommand {
    pub fn new(flags: ChatCommandFlags, engines: Engines) -> Self {
        Self { flags, engines }
    }

    pub fn flags(&self) -> &ChatCommandFlags {
        &self.flags
    }
}

#[async_trait]
impl Command for ChatCommand {
    type Frontend = Box<dyn ChatCommandFrontend>;
    type Outcome = ChatOutcome;

    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        let _ = self.engines;
        frontend.replay_queued();
        Ok(ChatOutcome {
            agent: self.flags.agent,
            exit_code: None,
        })
    }
}
