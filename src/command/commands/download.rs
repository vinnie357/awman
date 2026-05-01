//! `DownloadCommand` — placeholder. Per spec §4, `download` becomes an
//! internal helper consumed by `engine/agent/`; this command struct is
//! retained only for the structural Layer 2 surface in case the user-visible
//! `amux download` form is preserved later.

use async_trait::async_trait;
use serde::Serialize;

use crate::command::commands::Command;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::engine::message::UserMessageSink;

#[derive(Debug, Clone, Serialize)]
pub struct DownloadOutcome {
    pub asset: String,
}

pub trait DownloadCommandFrontend: UserMessageSink + Send + Sync {}

pub struct DownloadCommand {
    asset: String,
    engines: Engines,
}

impl DownloadCommand {
    pub fn new(asset: String, engines: Engines) -> Self {
        Self { asset, engines }
    }
}

#[async_trait]
impl Command for DownloadCommand {
    type Frontend = Box<dyn DownloadCommandFrontend>;
    type Outcome = DownloadOutcome;

    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        let _ = self.engines;
        frontend.replay_queued();
        Ok(DownloadOutcome { asset: self.asset })
    }
}
