//! `StatusCommand` — display the status of all running containers.

use async_trait::async_trait;
use serde::Serialize;

use crate::command::commands::Command;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::engine::message::UserMessageSink;

#[derive(Debug, Clone)]
pub struct StatusCommandFlags {
    pub watch: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusOutcome {
    pub containers: Vec<StatusContainerRow>,
    pub watched: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusContainerRow {
    pub id: String,
    pub name: String,
    pub image: String,
    pub started_at: String,
    pub tab_number: Option<u32>,
    pub stuck: bool,
    pub command_label: Option<String>,
}

/// Optional context supplied by the TUI; CLI / headless leave this `None`.
#[derive(Debug, Clone, Default)]
pub struct StatusCommandTuiContext {
    pub tabs: Vec<TuiTabSnapshot>,
}

#[derive(Debug, Clone)]
pub struct TuiTabSnapshot {
    pub tab_number: u32,
    pub container_name: Option<String>,
    pub is_stuck: bool,
    pub command_label: String,
}

pub trait StatusCommandFrontend: UserMessageSink + Send + Sync {
    /// Optional TUI context. Defaults to `None` for CLI / headless.
    fn tui_context(&self) -> Option<&StatusCommandTuiContext> {
        None
    }

    /// Whether the watch loop should continue. Default: stops after one tick.
    fn should_continue_watching(&mut self) -> bool {
        false
    }
}

pub struct StatusCommand {
    flags: StatusCommandFlags,
    engines: Engines,
}

impl StatusCommand {
    pub fn new(flags: StatusCommandFlags, engines: Engines) -> Self {
        Self { flags, engines }
    }

    pub fn flags(&self) -> &StatusCommandFlags {
        &self.flags
    }
}

#[async_trait]
impl Command for StatusCommand {
    type Frontend = Box<dyn StatusCommandFrontend>;
    type Outcome = StatusOutcome;

    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        let session = open_session()?;
        let handles = self
            .engines
            .runtime
            .list_running(&session)
            .map_err(CommandError::from)?;
        let context = frontend.tui_context().cloned();
        let containers = handles
            .into_iter()
            .map(|h| {
                let mut row = StatusContainerRow {
                    id: h.id.clone(),
                    name: h.name.clone(),
                    image: h.image_tag.clone(),
                    started_at: h.started_at.to_rfc3339(),
                    tab_number: None,
                    stuck: false,
                    command_label: None,
                };
                if let Some(ctx) = context.as_ref() {
                    if let Some(t) = ctx
                        .tabs
                        .iter()
                        .find(|t| t.container_name.as_deref() == Some(&h.name))
                    {
                        row.tab_number = Some(t.tab_number);
                        row.stuck = t.is_stuck;
                        row.command_label = Some(t.command_label.clone());
                    }
                }
                row
            })
            .collect();
        frontend.replay_queued();
        Ok(StatusOutcome {
            containers,
            watched: self.flags.watch,
        })
    }
}

impl StatusCommandTuiContext {
    pub fn new(tabs: Vec<TuiTabSnapshot>) -> Self {
        Self { tabs }
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
