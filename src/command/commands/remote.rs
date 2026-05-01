//! `RemoteCommand` — `remote run | session start | session kill`.

use async_trait::async_trait;
use serde::Serialize;

use crate::command::commands::Command;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::engine::message::UserMessageSink;

#[derive(Debug, Clone)]
pub struct RemoteRunFlags {
    pub command: Vec<String>,
    pub remote_addr: Option<String>,
    pub session: Option<String>,
    pub follow: bool,
    pub api_key: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RemoteSessionStartFlags {
    pub dir: Option<String>,
    pub remote_addr: Option<String>,
    pub api_key: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RemoteSessionKillFlags {
    pub session_id: Option<String>,
    pub remote_addr: Option<String>,
    pub api_key: Option<String>,
}

#[derive(Debug, Clone)]
pub enum RemoteSubcommand {
    Run(RemoteRunFlags),
    SessionStart(RemoteSessionStartFlags),
    SessionKill(RemoteSessionKillFlags),
}

#[derive(Debug, Clone, Serialize)]
pub struct RemoteRunOutcome {
    pub command: Vec<String>,
    pub session: Option<String>,
    pub remote_addr: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RemoteSessionStartOutcome {
    pub dir: Option<String>,
    pub remote_addr: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RemoteSessionKillOutcome {
    pub session_id: Option<String>,
    pub remote_addr: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", content = "payload")]
pub enum RemoteOutcome {
    Run(RemoteRunOutcome),
    SessionStart(RemoteSessionStartOutcome),
    SessionKill(RemoteSessionKillOutcome),
}

pub trait RemoteCommandFrontend: UserMessageSink + Send + Sync {}

pub struct RemoteCommand {
    sub: RemoteSubcommand,
    engines: Engines,
}

impl RemoteCommand {
    pub fn new(sub: RemoteSubcommand, engines: Engines) -> Self {
        Self { sub, engines }
    }

    pub fn subcommand(&self) -> &RemoteSubcommand {
        &self.sub
    }
}

#[async_trait]
impl Command for RemoteCommand {
    type Frontend = Box<dyn RemoteCommandFrontend>;
    type Outcome = RemoteOutcome;

    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        let _ = self.engines;
        let outcome = match self.sub {
            RemoteSubcommand::Run(f) => RemoteOutcome::Run(RemoteRunOutcome {
                command: f.command,
                session: f.session,
                remote_addr: f.remote_addr,
            }),
            RemoteSubcommand::SessionStart(f) => {
                RemoteOutcome::SessionStart(RemoteSessionStartOutcome {
                    dir: f.dir,
                    remote_addr: f.remote_addr,
                })
            }
            RemoteSubcommand::SessionKill(f) => {
                RemoteOutcome::SessionKill(RemoteSessionKillOutcome {
                    session_id: f.session_id,
                    remote_addr: f.remote_addr,
                })
            }
        };
        frontend.replay_queued();
        Ok(outcome)
    }
}
