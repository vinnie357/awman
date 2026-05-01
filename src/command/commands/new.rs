//! `NewCommand` — `new spec`, `new workflow`, `new skill`.

use async_trait::async_trait;
use serde::Serialize;

use crate::command::commands::Command;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::engine::message::UserMessageSink;

#[derive(Debug, Clone)]
pub struct NewSpecFlags {
    pub interview: bool,
}

#[derive(Debug, Clone)]
pub struct NewWorkflowFlags {
    pub interview: bool,
    pub global: bool,
    pub format: String,
}

#[derive(Debug, Clone)]
pub struct NewSkillFlags {
    pub interview: bool,
    pub global: bool,
}

#[derive(Debug, Clone)]
pub enum NewSubcommand {
    Spec(NewSpecFlags),
    Workflow(NewWorkflowFlags),
    Skill(NewSkillFlags),
}

#[derive(Debug, Clone, Serialize)]
pub struct NewSpecOutcome {
    pub interview: bool,
    pub path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NewWorkflowOutcome {
    pub interview: bool,
    pub global: bool,
    pub format: String,
    pub path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NewSkillOutcome {
    pub interview: bool,
    pub global: bool,
    pub path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", content = "payload")]
pub enum NewOutcome {
    Spec(NewSpecOutcome),
    Workflow(NewWorkflowOutcome),
    Skill(NewSkillOutcome),
}

pub trait NewCommandFrontend: UserMessageSink + Send + Sync {}

pub struct NewCommand {
    sub: NewSubcommand,
    engines: Engines,
}

impl NewCommand {
    pub fn new(sub: NewSubcommand, engines: Engines) -> Self {
        Self { sub, engines }
    }

    pub fn subcommand(&self) -> &NewSubcommand {
        &self.sub
    }
}

#[async_trait]
impl Command for NewCommand {
    type Frontend = Box<dyn NewCommandFrontend>;
    type Outcome = NewOutcome;

    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        let _ = self.engines;
        let outcome = match self.sub {
            NewSubcommand::Spec(f) => NewOutcome::Spec(NewSpecOutcome {
                interview: f.interview,
                path: None,
            }),
            NewSubcommand::Workflow(f) => NewOutcome::Workflow(NewWorkflowOutcome {
                interview: f.interview,
                global: f.global,
                format: f.format,
                path: None,
            }),
            NewSubcommand::Skill(f) => NewOutcome::Skill(NewSkillOutcome {
                interview: f.interview,
                global: f.global,
                path: None,
            }),
        };
        frontend.replay_queued();
        Ok(outcome)
    }
}
