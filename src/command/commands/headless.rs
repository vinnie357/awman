//! `HeadlessCommand` — `headless start | kill | logs | status`.

use async_trait::async_trait;
use serde::Serialize;

use crate::command::commands::Command;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::engine::message::UserMessageSink;

pub mod banner;

#[derive(Debug, Clone)]
pub struct HeadlessStartFlags {
    pub port: u16,
    pub workdirs: Vec<String>,
    pub background: bool,
    pub refresh_key: bool,
    pub dangerously_skip_auth: bool,
}

#[derive(Debug, Clone)]
pub struct HeadlessKillFlags {}

#[derive(Debug, Clone)]
pub struct HeadlessLogsFlags {}

#[derive(Debug, Clone)]
pub struct HeadlessStatusFlags {}

#[derive(Debug, Clone)]
pub enum HeadlessSubcommand {
    Start(HeadlessStartFlags),
    Kill(HeadlessKillFlags),
    Logs(HeadlessLogsFlags),
    Status(HeadlessStatusFlags),
}

#[derive(Debug, Clone, Serialize)]
pub struct HeadlessStartOutcome {
    pub port: u16,
    pub background: bool,
    pub workdirs: Vec<String>,
    pub refreshed_key: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct HeadlessKillOutcome {
    pub stopped_pid: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HeadlessLogsOutcome {
    pub log_path: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct HeadlessStatusOutcome {
    pub running: bool,
    pub pid: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", content = "payload")]
pub enum HeadlessOutcome {
    Start(HeadlessStartOutcome),
    Kill(HeadlessKillOutcome),
    Logs(HeadlessLogsOutcome),
    Status(HeadlessStatusOutcome),
}

/// Methods Layer 3 must provide to the headless start command. Wired up in
/// 0069 against the actual axum server.
pub trait HeadlessStartCommandFrontend: UserMessageSink + Send + Sync {
    /// Hand off the assembled config to the frontend's HTTP server. Returns
    /// when the server shuts down.
    fn serve_until_shutdown(&mut self) -> Result<(), CommandError>;
}

pub trait HeadlessKillCommandFrontend: UserMessageSink + Send + Sync {}
pub trait HeadlessLogsCommandFrontend: UserMessageSink + Send + Sync {}
pub trait HeadlessStatusCommandFrontend: UserMessageSink + Send + Sync {}

/// Catch-all frontend for the umbrella `HeadlessCommand`.
pub trait HeadlessCommandFrontend: UserMessageSink + Send + Sync {}

pub struct HeadlessCommand {
    sub: HeadlessSubcommand,
    engines: Engines,
}

impl HeadlessCommand {
    pub fn new(sub: HeadlessSubcommand, engines: Engines) -> Self {
        Self { sub, engines }
    }

    pub fn subcommand(&self) -> &HeadlessSubcommand {
        &self.sub
    }
}

#[async_trait]
impl Command for HeadlessCommand {
    type Frontend = Box<dyn HeadlessCommandFrontend>;
    type Outcome = HeadlessOutcome;

    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        let _ = self.engines;
        let outcome = match self.sub {
            HeadlessSubcommand::Start(f) => HeadlessOutcome::Start(HeadlessStartOutcome {
                port: f.port,
                background: f.background,
                workdirs: f.workdirs,
                refreshed_key: f.refresh_key,
            }),
            HeadlessSubcommand::Kill(_) => {
                HeadlessOutcome::Kill(HeadlessKillOutcome { stopped_pid: None })
            }
            HeadlessSubcommand::Logs(_) => HeadlessOutcome::Logs(HeadlessLogsOutcome {
                log_path: String::new(),
            }),
            HeadlessSubcommand::Status(_) => HeadlessOutcome::Status(HeadlessStatusOutcome {
                running: false,
                pid: None,
            }),
        };
        frontend.replay_queued();
        Ok(outcome)
    }
}

/// Resolve the merged-and-validated workdir allowlist (per spec §6.4a).
/// Concatenate CLI-supplied workdirs and config workdirs, canonicalize,
/// deduplicate, and reject missing paths.
pub fn resolve_workdirs(
    cli: &[String],
    config: &[String],
) -> Result<Vec<std::path::PathBuf>, CommandError> {
    use std::collections::BTreeSet;
    let mut seen: BTreeSet<std::path::PathBuf> = BTreeSet::new();
    let mut out: Vec<std::path::PathBuf> = Vec::new();
    for raw in cli.iter().chain(config.iter()) {
        let path = std::path::PathBuf::from(raw);
        if !path.exists() {
            return Err(CommandError::HeadlessWorkdirNotFound { path });
        }
        let canon = path.canonicalize().unwrap_or(path);
        if seen.insert(canon.clone()) {
            out.push(canon);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_workdirs_dedupes_overlapping_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let s = tmp.path().to_str().unwrap().to_string();
        let merged = resolve_workdirs(&[s.clone()], &[s.clone()]).unwrap();
        assert_eq!(merged.len(), 1);
    }

    #[test]
    fn resolve_workdirs_errors_on_missing_path() {
        let err = resolve_workdirs(&["/no/such/path".into()], &[]).unwrap_err();
        assert!(matches!(err, CommandError::HeadlessWorkdirNotFound { .. }));
    }
}
