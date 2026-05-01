//! `SpecsCommand` — `specs new` and `specs amend`.

use async_trait::async_trait;
use serde::Serialize;

use crate::command::commands::Command;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::engine::message::UserMessageSink;

#[derive(Debug, Clone)]
pub struct SpecsNewFlags {
    pub interview: bool,
}

#[derive(Debug, Clone)]
pub struct SpecsAmendFlags {
    pub work_item: String,
    pub non_interactive: bool,
    pub allow_docker: bool,
}

#[derive(Debug, Clone)]
pub enum SpecsSubcommand {
    New(SpecsNewFlags),
    Amend(SpecsAmendFlags),
}

#[derive(Debug, Clone, Serialize)]
pub struct SpecsNewOutcome {
    pub interview: bool,
    pub created_path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SpecsAmendOutcome {
    pub work_item: String,
    pub non_interactive: bool,
    pub allow_docker: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", content = "payload")]
pub enum SpecsOutcome {
    New(SpecsNewOutcome),
    Amend(SpecsAmendOutcome),
}

pub trait SpecsCommandFrontend: UserMessageSink + Send + Sync {}

pub struct SpecsCommand {
    sub: SpecsSubcommand,
    engines: Engines,
}

impl SpecsCommand {
    pub fn new(sub: SpecsSubcommand, engines: Engines) -> Self {
        Self { sub, engines }
    }

    pub fn subcommand(&self) -> &SpecsSubcommand {
        &self.sub
    }
}

#[async_trait]
impl Command for SpecsCommand {
    type Frontend = Box<dyn SpecsCommandFrontend>;
    type Outcome = SpecsOutcome;

    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        let _ = self.engines;
        let outcome = match self.sub {
            SpecsSubcommand::New(f) => SpecsOutcome::New(SpecsNewOutcome {
                interview: f.interview,
                created_path: None,
            }),
            SpecsSubcommand::Amend(f) => SpecsOutcome::Amend(SpecsAmendOutcome {
                work_item: f.work_item,
                non_interactive: f.non_interactive,
                allow_docker: f.allow_docker,
            }),
        };
        frontend.replay_queued();
        Ok(outcome)
    }
}
