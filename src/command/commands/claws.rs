//! `ClawsCommand` — thin wrapper over `ClawsEngine`.

use async_trait::async_trait;
use serde::Serialize;

use crate::command::commands::Command;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::engine::claws::{
    ClawsEngine, ClawsEngineOptions, ClawsFrontend, ClawsMode, ClawsSummary,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClawsCommandMode {
    Init,
    Ready,
    Chat,
}

impl From<ClawsCommandMode> for ClawsMode {
    fn from(m: ClawsCommandMode) -> Self {
        match m {
            ClawsCommandMode::Init => ClawsMode::Init,
            ClawsCommandMode::Ready => ClawsMode::Ready,
            ClawsCommandMode::Chat => ClawsMode::Chat,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ClawsCommandFlags {
    pub mode: ClawsCommandMode,
}

#[derive(Debug, Clone, Serialize)]
pub struct ClawsOutcome {
    pub mode: String,
    pub clone: String,
    pub permissions_check: String,
    pub image_build: String,
    pub audit: String,
    pub configure: String,
    pub controller: String,
}

impl From<(ClawsCommandMode, ClawsSummary)> for ClawsOutcome {
    fn from((mode, s): (ClawsCommandMode, ClawsSummary)) -> Self {
        Self {
            mode: format!("{mode:?}"),
            clone: format!("{:?}", s.clone),
            permissions_check: format!("{:?}", s.permissions_check),
            image_build: format!("{:?}", s.image_build),
            audit: format!("{:?}", s.audit),
            configure: format!("{:?}", s.configure),
            controller: format!("{:?}", s.controller),
        }
    }
}

pub trait ClawsCommandFrontend: ClawsFrontend + Send {}
impl<T: ClawsFrontend + Send> ClawsCommandFrontend for T {}

pub struct ClawsCommand {
    flags: ClawsCommandFlags,
    engines: Engines,
}

impl ClawsCommand {
    pub fn new(flags: ClawsCommandFlags, engines: Engines) -> Self {
        Self { flags, engines }
    }

    pub fn flags(&self) -> &ClawsCommandFlags {
        &self.flags
    }
}

#[async_trait]
impl Command for ClawsCommand {
    type Frontend = Box<dyn ClawsCommandFrontend>;
    type Outcome = ClawsOutcome;

    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        let session = open_session()?;
        let clone_dir = std::env::temp_dir().join("nanoclaw");
        let mode = self.flags.mode;
        let mut engine = ClawsEngine::new(
            std::sync::Arc::new(session),
            self.engines.git_engine.clone(),
            self.engines.overlay_engine.clone(),
            self.engines.runtime.clone(),
            ClawsEngineOptions {
                mode: mode.into(),
                nanoclaw_url: None,
                refresh: false,
                no_cache: false,
                clone_dir,
            },
        );
        let summary = engine
            .run_to_completion(frontend.as_mut())
            .await
            .map_err(CommandError::from)?;
        frontend.replay_queued();
        Ok((mode, summary).into())
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
