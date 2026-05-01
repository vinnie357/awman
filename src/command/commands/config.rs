//! `ConfigCommand` — view and edit global / repo configuration.

use async_trait::async_trait;
use serde::Serialize;

use crate::command::commands::Command;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::engine::message::UserMessageSink;

#[derive(Debug, Clone)]
pub struct ConfigShowFlags {}

#[derive(Debug, Clone)]
pub struct ConfigGetFlags {
    pub field: String,
}

#[derive(Debug, Clone)]
pub struct ConfigSetFlags {
    pub field: String,
    pub value: String,
    pub global: bool,
}

#[derive(Debug, Clone)]
pub enum ConfigSubcommand {
    Show(ConfigShowFlags),
    Get(ConfigGetFlags),
    Set(ConfigSetFlags),
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigShowOutcome {
    pub global: serde_json::Value,
    pub repo: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigGetOutcome {
    pub field: String,
    pub global_value: Option<String>,
    pub repo_value: Option<String>,
    pub effective_value: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigSetOutcome {
    pub field: String,
    pub value: String,
    pub scope: String,
}

pub trait ConfigCommandFrontend: UserMessageSink + Send + Sync {}

pub struct ConfigCommand {
    sub: ConfigSubcommand,
    engines: Engines,
}

impl ConfigCommand {
    pub fn new(sub: ConfigSubcommand, engines: Engines) -> Self {
        Self { sub, engines }
    }

    pub fn subcommand(&self) -> &ConfigSubcommand {
        &self.sub
    }
}

/// Outcome enum used by the `Command` trait impl.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", content = "payload")]
pub enum ConfigOutcome {
    Show(ConfigShowOutcome),
    Get(ConfigGetOutcome),
    Set(ConfigSetOutcome),
}

#[async_trait]
impl Command for ConfigCommand {
    type Frontend = Box<dyn ConfigCommandFrontend>;
    type Outcome = ConfigOutcome;

    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        let _ = self.engines;
        let session = open_session()?;
        let outcome = match self.sub {
            ConfigSubcommand::Show(_) => ConfigOutcome::Show(ConfigShowOutcome {
                global: serde_json::to_value(session.global_config()).unwrap_or(serde_json::Value::Null),
                repo: serde_json::to_value(session.repo_config()).unwrap_or(serde_json::Value::Null),
            }),
            ConfigSubcommand::Get(f) => ConfigOutcome::Get(ConfigGetOutcome {
                field: f.field,
                global_value: None,
                repo_value: None,
                effective_value: None,
            }),
            ConfigSubcommand::Set(f) => ConfigOutcome::Set(ConfigSetOutcome {
                field: f.field,
                value: f.value,
                scope: if f.global { "global".into() } else { "repo".into() },
            }),
        };
        frontend.replay_queued();
        Ok(outcome)
    }
}

fn open_session() -> Result<crate::data::session::Session, CommandError> {
    let cwd = std::env::current_dir()
        .map_err(|e| CommandError::Other(format!("cwd unavailable: {e}")))?;
    let resolver = crate::data::session::StaticGitRootResolver::new(cwd.clone());
    crate::data::session::Session::open(
        cwd,
        &resolver,
        crate::data::session::SessionOpenOptions::default(),
    )
    .map_err(CommandError::from)
}
