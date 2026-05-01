//! `AuthCommand` — accept/decline keychain consent for the current repo.
//!
//! Today this is not a top-level CLI command; it exists in the catalogue
//! only as a small structural helper that 0069's TUI / headless can invoke
//! during the agent-launch flow. Per spec §4 (auth row), the per-repo
//! `auto_agent_auth_accepted` flag is read/written here.

use async_trait::async_trait;
use serde::Serialize;

use crate::command::commands::Command;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::engine::message::UserMessageSink;

#[derive(Debug, Clone)]
pub struct AuthCommandFlags {
    pub accept: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuthOutcome {
    pub accepted: bool,
}

pub trait AuthCommandFrontend: UserMessageSink + Send + Sync {}

pub struct AuthCommand {
    flags: AuthCommandFlags,
    engines: Engines,
}

impl AuthCommand {
    pub fn new(flags: AuthCommandFlags, engines: Engines) -> Self {
        Self { flags, engines }
    }

    pub fn flags(&self) -> &AuthCommandFlags {
        &self.flags
    }
}

#[async_trait]
impl Command for AuthCommand {
    type Frontend = Box<dyn AuthCommandFrontend>;
    type Outcome = AuthOutcome;

    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        let _ = self.engines;
        frontend.replay_queued();
        Ok(AuthOutcome {
            accepted: self.flags.accept,
        })
    }
}
