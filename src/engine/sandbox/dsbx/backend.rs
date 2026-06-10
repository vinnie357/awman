//! `DSbxBackend` — the Docker Sandboxes (`sbx`) driver.
//!
//! Replaces the WI 0089 stub with the real `sbx`-driven implementation:
//! lifecycle (`run`/`create`/`stop`/`rm`), listing (`sbx ls --json`),
//! exec, and the interactive PTY-bridged launch ([`run_interactive`]).
//!
//! Every non-interactive `sbx` invocation goes through [`super::spawn`]; the
//! interactive `sbx run` PTY session is bridged by [`super::io_bridge`] after
//! its argv is announced on the sink.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::Utc;

use crate::data::fs::SandboxKitPaths;
use crate::data::session::AgentHandle;
use crate::engine::agent::agent_matrix::{matrix_for, SbxKitKind};
use crate::engine::agent_runtime::execution::{
    AgentExecution, AgentExitInfo, AgentStats, ExecutionBackend,
};
use crate::engine::agent_runtime::frontend::{AgentFrontend, AgentStatus};
use crate::engine::agent_runtime::ExecOutput;
use crate::engine::error::EngineError;
use crate::engine::sandbox::backend::{SandboxBackend, SandboxId};
use crate::engine::sandbox::dsbx::auth;
use crate::engine::sandbox::dsbx::io_bridge;
use crate::engine::sandbox::dsbx::session_config::DSbxSessionConfig;
use crate::engine::sandbox::dsbx::spawn::{SbxCommand, SBX_BIN};
use crate::engine::sandbox::naming::sandbox_name_for;
use crate::engine::sandbox::options::ResolvedSandboxOptions;

#[derive(Debug, Default)]
pub(in crate::engine::sandbox) struct DSbxBackend;

impl DSbxBackend {
    pub(in crate::engine::sandbox) fn new() -> Self {
        Self
    }
}

impl SandboxBackend for DSbxBackend {
    fn start_sandbox(&self, opts: &ResolvedSandboxOptions) -> Result<SandboxId, EngineError> {
        let agent = require_agent(opts)?;
        let name = resolve_sandbox_name(opts);
        let kit_dir = kit_dir_for(&agent)?;
        // Background create writes the per-launch config but cannot inject
        // credentials (no sink to report through); the interactive launch path
        // and `awman ready` own credential registration.
        DSbxSessionConfig::write_for(opts, &opts.workspace_dir)?;
        let mut argv = vec![
            "create".to_string(),
            "--kit".to_string(),
            kit_dir.display().to_string(),
            "--name".to_string(),
            name.clone(),
            "--workspace-dir".to_string(),
            opts.workspace_dir.display().to_string(),
        ];
        if let Some(mem) = opts.memory_gb {
            argv.push("--memory".to_string());
            argv.push(format!("{mem}g"));
        }
        argv.push(agent);
        SbxCommand::new(argv).run_quiet()?;
        Ok(SandboxId::new(name))
    }

    fn restart_sandbox(&self, id: &SandboxId) -> Result<(), EngineError> {
        let agent = agent_from_name(id.as_str());
        SbxCommand::new(["run", "--name", id.as_str(), &agent]).run_quiet()?;
        Ok(())
    }

    fn exec_in_sandbox(
        &self,
        id: &SandboxId,
        command: &str,
        working_dir: &str,
        env: Option<&HashMap<String, String>>,
    ) -> Result<ExecOutput, EngineError> {
        let mut argv = vec!["exec".to_string()];
        if let Some(env) = env {
            for (k, v) in env {
                argv.push("--env".to_string());
                argv.push(format!("{k}={v}"));
            }
        }
        argv.push(id.0.clone());
        argv.push("sh".to_string());
        argv.push("-lc".to_string());
        argv.push(format!("cd {} && {command}", shell_quote(working_dir)));
        let out = SbxCommand::new(argv).run_quiet()?;
        Ok(ExecOutput {
            stdout: out.stdout,
            stderr: out.stderr,
            exit_code: out.exit_code,
        })
    }

    fn stop(&self, handle: &AgentHandle) -> Result<(), EngineError> {
        // `sbx stop` pauses the VM and preserves its persistent volume.
        // Best-effort: a non-zero exit (already stopped) is not an error.
        let _ = SbxCommand::new(["stop", &handle.name]).run_quiet();
        Ok(())
    }

    fn remove(&self, id: &SandboxId) -> Result<(), EngineError> {
        // `sbx rm` deletes the VM and its persistent volume.
        let _ = SbxCommand::new(["rm", id.as_str()]).run_quiet();
        Ok(())
    }

    fn list_running(&self) -> Result<Vec<AgentHandle>, EngineError> {
        Ok(list_awman_sandboxes())
    }

    fn stats(&self, handle: &AgentHandle) -> Result<AgentStats, EngineError> {
        // Sandboxes run the agent directly in the VM (not as a Docker
        // container), so there are no per-resource metrics to poll. Report
        // zeros with the sandbox name, per the unified AgentStats shape.
        Ok(AgentStats {
            name: handle.name.clone(),
            cpu_percent: 0.0,
            memory_mb: 0.0,
        })
    }

    fn name(&self) -> &'static str {
        "docker-sbx-experimental"
    }

    fn cli_binary(&self) -> &'static str {
        SBX_BIN
    }
}

// ─── Naming / kit-dir helpers ───────────────────────────────────────────────

/// Emit a Warning-level message on a sink trait object (the `warning`
/// convenience method is `Self: Sized` and so unavailable on `dyn`).
fn warn(sink: &mut dyn crate::data::message::UserMessageSink, text: String) {
    sink.write_message(crate::data::message::UserMessage {
        level: crate::data::message::MessageLevel::Warning,
        text,
    });
}

/// Single-quote a string for `sh -c`, escaping embedded single quotes.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn require_agent(opts: &ResolvedSandboxOptions) -> Result<String, EngineError> {
    if opts.agent_id.is_empty() {
        return Err(EngineError::Sandbox(
            "sandbox launch requires an agent id".into(),
        ));
    }
    Ok(opts.agent_id.clone())
}

/// Resolve the deterministic sandbox name. Uses a caller-supplied name when
/// present, otherwise derives `awman-<worktree-hash>-<agent>`.
fn resolve_sandbox_name(opts: &ResolvedSandboxOptions) -> String {
    opts.sandbox_name
        .clone()
        .unwrap_or_else(|| sandbox_name_for(&opts.workspace_dir, &opts.agent_id))
}

/// Recover the agent name from a `awman-<hash>-<agent>` sandbox name.
fn agent_from_name(name: &str) -> String {
    name.rsplit('-').next().unwrap_or(name).to_string()
}

fn kit_dir_for(agent: &str) -> Result<PathBuf, EngineError> {
    Ok(SandboxKitPaths::from_process_env()?.kit_dir(agent))
}

fn kit_kind_for(agent: &str) -> SbxKitKind {
    matrix_for(agent)
        .map(|m| m.sbx_kit_kind)
        .unwrap_or(SbxKitKind::Mixin)
}

// ─── Listing ────────────────────────────────────────────────────────────────

/// All awman-owned sandbox handles from `sbx ls`.
fn list_awman_sandboxes() -> Vec<AgentHandle> {
    list_all_sandbox_names()
        .into_iter()
        .filter(|n| n.starts_with("awman-"))
        .map(|name| AgentHandle {
            id: name.clone(),
            image_tag: String::new(),
            name,
            started_at: Utc::now(),
        })
        .collect()
}

/// Every sandbox name `sbx ls` reports (awman-owned or not). Tolerant of both
/// the `--json` mode and the plain table fallback; returns an empty list when
/// `sbx` is unavailable or not logged in.
pub(super) fn list_all_sandbox_names() -> Vec<String> {
    if let Ok(out) = SbxCommand::new(["ls", "--json"]).run_quiet() {
        if out.success() {
            let names = parse_ls_json(&out.stdout);
            if !names.is_empty() || out.stdout.trim() == "[]" {
                return names;
            }
        }
    }
    match SbxCommand::new(["ls"]).run_quiet() {
        Ok(out) if out.success() => parse_ls_table(&out.stdout),
        _ => Vec::new(),
    }
}

/// Parse `sbx ls --json`. The exact schema is unverified as of June 2026
/// (Phase 0), so this accepts either a top-level array or an object wrapping
/// a `sandboxes` array, and reads `name` / `Name`.
fn parse_ls_json(stdout: &str) -> Vec<String> {
    let value: serde_json::Value = match serde_json::from_str(stdout) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let array = value
        .as_array()
        .cloned()
        .or_else(|| value.get("sandboxes").and_then(|s| s.as_array().cloned()))
        .unwrap_or_default();
    array
        .iter()
        .filter_map(|item| {
            item.get("name")
                .or_else(|| item.get("Name"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .collect()
}

/// Fallback parser for the plain `sbx ls` table: pick the first
/// `awman-`-prefixed whitespace token on each line.
fn parse_ls_table(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .skip(1) // header row
        .filter_map(|line| {
            line.split_whitespace()
                .find(|tok| tok.starts_with("awman-"))
                .map(|s| s.to_string())
        })
        .collect()
}

fn sandbox_name_exists(name: &str) -> bool {
    list_all_sandbox_names().iter().any(|n| n == name)
}

// ─── Interactive launch ─────────────────────────────────────────────────────

/// Perform the full interactive launch: write `session.json`, inject
/// credentials, build the `sbx run` argv (first launch vs restart), announce
/// it, and bridge the PTY/piped I/O to the frontend.
///
/// This is the sandbox tier's equivalent of `DockerContainerInstance::
/// run_with_frontend`. It is reached from `SandboxRuntime`'s `AgentInstance`.
pub(in crate::engine::sandbox) fn run_interactive(
    options: ResolvedSandboxOptions,
    mut frontend: Box<dyn AgentFrontend>,
) -> Result<AgentExecution, EngineError> {
    let agent = require_agent(&options)?;
    let name = resolve_sandbox_name(&options);
    let kit_dir = kit_dir_for(&agent)?;

    // 1. Per-launch session config (no credential values).
    DSbxSessionConfig::write_for(&options, &options.workspace_dir)?;

    // 2. Surface unsupported / withheld options.
    if options.cpu_limit.is_some() {
        warn(
            &mut *frontend,
            "sbx: CPU limits are not supported by Docker Sandboxes; ignoring.".to_string(),
        );
    }
    // Outside-workspace overlays are rejected with a clear error (WI 0090):
    // the VM can only see virtiofs-mounted workspace paths, and silently
    // launching without the overlay would surprise the user.
    for overlay in &options.extra_overlays {
        if !overlay.host_path.starts_with(&options.workspace_dir) {
            return Err(EngineError::Sandbox(format!(
                "overlay '{}' is outside the workspace and cannot be mounted into the sandbox \
                 VM; move it inside the workspace or remove the overlay when using \
                 docker-sbx-experimental",
                overlay.host_path.display()
            )));
        }
    }
    for text in withheld_env_warnings(&options) {
        warn(&mut *frontend, text);
    }
    for note in &options.unsupported_notes {
        warn(&mut *frontend, format!("sbx: {note}"));
    }

    // 3. Register credentials with `sbx secret set` (announced + redacted).
    auth::inject_credentials(&options.agent_credentials, &mut *frontend)?;

    // 4. Build the launch argv: restart an existing sandbox (no --kit) or
    //    create a fresh one.
    let restart = sandbox_name_exists(&name);
    let argv = if restart {
        restart_argv(&name, &agent, &options)
    } else {
        first_launch_argv(&name, &agent, &kit_dir, &options)
    };

    let started_at = Utc::now();
    let handle = AgentHandle {
        id: name.clone(),
        image_tag: agent.clone(),
        name: name.clone(),
        started_at,
    };

    frontend.report_status(AgentStatus::Running {
        container_name: name.clone(),
    });

    // 5. Announce the command, then bridge I/O.
    crate::engine::sandbox::dsbx::spawn::announce(
        &mut *frontend,
        &format!("{SBX_BIN} {}", argv.join(" ")),
    );

    let io = frontend.take_io();
    let seed = stdin_seed(&agent, &options);
    if io.initial_size.is_some() {
        spawn_pty_bridged(name, argv, seed, started_at, handle, io)
    } else {
        spawn_piped(name, argv, seed, started_at, handle, io)
    }
}

/// The seeded prompt to write into the launch's stdin, or `None`.
///
/// `kind: agent` kits already carry the prompt as a positional launch arg
/// (`append_seeded_positional`), so writing it to stdin too would deliver it
/// twice. `kind: mixin` kits launch via Docker's built-in template — the
/// apply script can only stage the prompt, not deliver it — so stdin
/// injection is their single delivery path (PTY type-ahead when interactive,
/// piped stdin otherwise).
fn stdin_seed(agent: &str, options: &ResolvedSandboxOptions) -> Option<String> {
    match kit_kind_for(agent) {
        SbxKitKind::Agent => None,
        SbxKitKind::Mixin => options.seeded_prompt.clone(),
    }
}

/// Warnings for credential-class env vars that arrive via the env channels.
/// They are withheld from the workspace-readable `session.json` (never
/// silently dropped — WI 0090 requires the user be told).
fn withheld_env_warnings(options: &ResolvedSandboxOptions) -> Vec<String> {
    options
        .env_passthrough
        .iter()
        .map(|v| v.0.as_str())
        .chain(options.env_literal.iter().map(|l| l.key.as_str()))
        .filter(|key| auth::is_credential_like(key))
        .map(|key| match auth::service_for_credential(key) {
            Some(service) => format!(
                "sbx: env var '{key}' is credential-class and was withheld from the \
                 workspace-readable session.json; it reaches the VM via `sbx secret set` \
                 ({service}) when resolved by awman's auth."
            ),
            None => format!(
                "sbx: env var '{key}' looks credential-like and was withheld from \
                 session.json. Register it with `sbx secret set` or the kit's \
                 `environment.proxyManaged` instead."
            ),
        })
        .collect()
}

/// First-launch argv: `sbx run --kit <dir> --name <name> --workspace-dir <wd>
/// [--memory Ng] <agent> [seeded prompt]`.
fn first_launch_argv(
    name: &str,
    agent: &str,
    kit_dir: &Path,
    options: &ResolvedSandboxOptions,
) -> Vec<String> {
    let mut argv = vec![
        "run".to_string(),
        "--kit".to_string(),
        kit_dir.display().to_string(),
        "--name".to_string(),
        name.to_string(),
        "--workspace-dir".to_string(),
        options.workspace_dir.display().to_string(),
    ];
    if let Some(mem) = options.memory_gb {
        argv.push("--memory".to_string());
        argv.push(format!("{mem}g"));
    }
    argv.push(agent.to_string());
    append_seeded_positional(&mut argv, agent, options);
    argv
}

/// Restart argv: `sbx run --name <name> <agent> [seeded prompt]` (the kit is
/// already baked into the existing sandbox, so no `--kit`).
fn restart_argv(name: &str, agent: &str, options: &ResolvedSandboxOptions) -> Vec<String> {
    let mut argv = vec![
        "run".to_string(),
        "--name".to_string(),
        name.to_string(),
        agent.to_string(),
    ];
    append_seeded_positional(&mut argv, agent, options);
    argv
}

/// For `kind: agent` kits the seeded prompt is appended as a positional arg to
/// the agent launch; for `kind: mixin` it is delivered via `session.json` and
/// the startup script, so it is NOT appended here.
fn append_seeded_positional(argv: &mut Vec<String>, agent: &str, options: &ResolvedSandboxOptions) {
    if let Some(prompt) = &options.seeded_prompt {
        if matches!(kit_kind_for(agent), SbxKitKind::Agent) {
            argv.push(prompt.clone());
        }
    }
}

fn spawn_pty_bridged(
    name: String,
    argv: Vec<String>,
    seeded: Option<String>,
    started_at: chrono::DateTime<Utc>,
    handle: AgentHandle,
    io: crate::engine::agent_runtime::frontend::AgentIo,
) -> Result<AgentExecution, EngineError> {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};

    let (cols, rows) = io.initial_size.expect("PTY path requires initial_size");
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| EngineError::Sandbox(format!("openpty: {e}")))?;

    let mut cmd = CommandBuilder::new(SBX_BIN);
    for arg in &argv {
        cmd.arg(arg);
    }
    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| EngineError::Sandbox(format!("spawn sbx via pty: {e}")))?;

    // Mixin seeded prompt: queue it on the stdin channel before the bridge
    // starts. The PTY delivers it as type-ahead input — the agent reads it
    // (with the trailing \r as Enter) once it starts accepting input.
    if let Some(prompt) = seeded {
        let _ = io.stdin_tx.send(prompt.into_bytes());
        let _ = io.stdin_tx.send(b"\r".to_vec());
    }

    let (master_arc, bridge) = io_bridge::bridge_pty(io, pair)?;

    let backend = DSbxExecution {
        child: None,
        pty_child: Some(child),
        pty_master: Some(master_arc),
        stdin_injector: Some(bridge.stdin_injector),
        sandbox_name: name,
        started_at,
    };
    Ok(AgentExecution::new(handle, Box::new(backend), bridge.stuck_tx))
}

fn spawn_piped(
    name: String,
    argv: Vec<String>,
    seeded: Option<String>,
    started_at: chrono::DateTime<Utc>,
    handle: AgentHandle,
    io: crate::engine::agent_runtime::frontend::AgentIo,
) -> Result<AgentExecution, EngineError> {
    use std::process::{Command, Stdio};

    let mut cmd = Command::new(SBX_BIN);
    cmd.args(&argv);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            EngineError::Sandbox(
                "`sbx` binary not found on PATH; install Docker Sandboxes \
                 (`brew install docker/tap/sbx`)"
                    .into(),
            )
        } else {
            EngineError::Sandbox(format!("spawn sbx: {e}"))
        }
    })?;

    // Write the seeded prompt into stdin before the writer task starts.
    if let Some(prompt) = seeded {
        let _ = io.stdin_tx.send(prompt.into_bytes());
        let _ = io.stdin_tx.send(b"\n".to_vec());
    }

    let bridge = io_bridge::bridge_piped(io, &mut child);
    // Drop the injector so the writer task closes the child's stdin after the
    // seeded prompt drains (matches the container piped path).
    drop(bridge.stdin_injector);

    let backend = DSbxExecution {
        child: Some(child),
        pty_child: None,
        pty_master: None,
        stdin_injector: None,
        sandbox_name: name,
        started_at,
    };
    Ok(AgentExecution::new(handle, Box::new(backend), bridge.stuck_tx))
}

// ─── Execution backend ──────────────────────────────────────────────────────

struct DSbxExecution {
    child: Option<std::process::Child>,
    pty_child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
    pty_master: Option<io_bridge::PtyMaster>,
    stdin_injector: Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
    sandbox_name: String,
    started_at: chrono::DateTime<Utc>,
}

impl ExecutionBackend for DSbxExecution {
    fn wait_blocking(mut self: Box<Self>) -> Result<AgentExitInfo, EngineError> {
        if let Some(mut child) = self.pty_child.take() {
            let status = child
                .wait()
                .map_err(|e| EngineError::Sandbox(format!("wait sbx (pty): {e}")))?;
            self.pty_master = None;
            let exit_code = status.exit_code().try_into().unwrap_or(-1);
            return Ok(AgentExitInfo {
                exit_code,
                signal: None,
                started_at: self.started_at,
                ended_at: Utc::now(),
            });
        }

        let mut child = self
            .child
            .take()
            .ok_or_else(|| EngineError::Sandbox("execution already waited".into()))?;
        let status = child
            .wait()
            .map_err(|e| EngineError::Sandbox(format!("wait sbx: {e}")))?;
        let exit_code = status.code().unwrap_or(-1);
        #[cfg(unix)]
        let signal = {
            use std::os::unix::process::ExitStatusExt;
            status.signal()
        };
        #[cfg(not(unix))]
        let signal = None;
        Ok(AgentExitInfo {
            exit_code,
            signal,
            started_at: self.started_at,
            ended_at: Utc::now(),
        })
    }

    fn try_inject_stdin(&self, bytes: &[u8]) -> Result<bool, EngineError> {
        if let Some(tx) = &self.stdin_injector {
            tx.send(bytes.to_vec())
                .map_err(|e| EngineError::Sandbox(format!("inject stdin: {e}")))?;
            return Ok(true);
        }
        Ok(false)
    }

    fn cancel(&self) -> Result<(), EngineError> {
        // Cancel preserves the persistent volume — `sbx stop`, never `sbx rm`.
        let _ = SbxCommand::new(["stop", &self.sandbox_name]).run_quiet();
        Ok(())
    }

    fn cancel_handle(&self) -> Option<crate::engine::agent_runtime::execution::CancelHandle> {
        let name = self.sandbox_name.clone();
        Some(crate::engine::agent_runtime::execution::CancelHandle::new(
            move || {
                let _ = SbxCommand::new(["stop", &name]).run_quiet();
                Ok(())
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::sandbox::naming::worktree_hash;
    use crate::engine::sandbox::options::SandboxOption;

    fn opts(list: Vec<SandboxOption>) -> ResolvedSandboxOptions {
        ResolvedSandboxOptions::resolve(list)
    }

    // ─── Identity ──────────────────────────────────────────────────────────

    #[test]
    fn name_is_correct() {
        assert_eq!(DSbxBackend::new().name(), "docker-sbx-experimental");
    }

    #[test]
    fn cli_binary_is_sbx() {
        assert_eq!(DSbxBackend::new().cli_binary(), "sbx");
    }

    // ─── Naming ────────────────────────────────────────────────────────────

    #[test]
    fn worktree_hash_is_deterministic() {
        let p = Path::new("/work/tree/a");
        assert_eq!(worktree_hash(p), worktree_hash(p));
        assert_ne!(worktree_hash(p), worktree_hash(Path::new("/work/tree/b")));
    }

    #[test]
    fn worktree_hash_is_stable_across_releases() {
        // Pinned FNV-1a value: if this changes, every existing persistent
        // sandbox would be orphaned on upgrade. Do not change the algorithm.
        assert_eq!(worktree_hash(Path::new("/work/tree/a")), "f94dc1c0");
    }

    #[test]
    fn resolve_sandbox_name_prefers_explicit() {
        let o = opts(vec![
            SandboxOption::AgentId("claude".into()),
            SandboxOption::SandboxName("awman-custom-claude".into()),
        ]);
        assert_eq!(resolve_sandbox_name(&o), "awman-custom-claude");
    }

    #[test]
    fn resolve_sandbox_name_generates_when_absent() {
        let o = opts(vec![SandboxOption::AgentId("gemini".into())]);
        let name = resolve_sandbox_name(&o);
        assert!(name.starts_with("awman-"));
        assert!(name.ends_with("-gemini"));
    }

    #[test]
    fn agent_from_name_extracts_suffix() {
        assert_eq!(agent_from_name("awman-deadbeef-claude"), "claude");
    }

    #[test]
    fn shell_quote_handles_spaces_and_quotes() {
        assert_eq!(shell_quote("/work tree/a"), "'/work tree/a'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }

    // ─── Outside-workspace overlay rejection ───────────────────────────────

    #[test]
    fn outside_workspace_overlay_is_rejected_with_clear_error() {
        use crate::engine::container::options::{OverlayPermission, OverlaySpec};

        use crate::data::message::UserMessage;
        use crate::engine::agent_runtime::frontend::{
            AgentFrontend, AgentIo, AgentProgress, AgentStatus,
        };

        struct NullFrontend;
        impl crate::data::message::UserMessageSink for NullFrontend {
            fn write_message(&mut self, _: UserMessage) {}
            fn replay_queued(&mut self) {}
        }
        impl AgentFrontend for NullFrontend {
            fn report_status(&mut self, _: AgentStatus) {}
            fn report_progress(&mut self, _: AgentProgress) {}
            fn take_io(&mut self) -> AgentIo {
                let (stdout, _) = tokio::sync::mpsc::unbounded_channel();
                let (stderr, _) = tokio::sync::mpsc::unbounded_channel();
                let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
                AgentIo {
                    stdout,
                    stderr,
                    stdin_tx,
                    stdin_rx,
                    resize: None,
                    initial_size: None,
                }
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let options = ResolvedSandboxOptions::resolve(vec![
            SandboxOption::AgentId("claude".into()),
            SandboxOption::WorkspaceDir(tmp.path().to_path_buf()),
            SandboxOption::ExtraOverlay(OverlaySpec {
                host_path: "/somewhere/else/reference".into(),
                container_path: "/mnt/reference".into(),
                permission: OverlayPermission::ReadOnly,
            }),
        ]);
        match run_interactive(options, Box::new(NullFrontend)) {
            Err(EngineError::Sandbox(msg)) => {
                assert!(
                    msg.contains("/somewhere/else/reference") && msg.contains("outside the workspace"),
                    "error must name the overlay and the reason: {msg}"
                );
            }
            Err(e) => panic!("expected Sandbox error for outside-workspace overlay, got: {e:?}"),
            Ok(_) => panic!("expected Sandbox error for outside-workspace overlay, got Ok"),
        }
    }

    // ─── Withheld credential-class env warnings ────────────────────────────

    #[test]
    fn withheld_env_warnings_cover_credential_like_keys_only() {
        use crate::engine::container::options::{EnvLiteral, EnvVar};
        let o = opts(vec![
            SandboxOption::EnvPassthrough(EnvVar("ANTHROPIC_API_KEY".into())),
            SandboxOption::EnvLiteral(EnvLiteral {
                key: "MY_DB_PASSWORD".into(),
                value: "hunter2".into(),
            }),
            SandboxOption::EnvLiteral(EnvLiteral {
                key: "LOG_LEVEL".into(),
                value: "debug".into(),
            }),
        ]);
        let warnings = withheld_env_warnings(&o);
        assert_eq!(warnings.len(), 2, "one warning per credential-like key: {warnings:?}");
        let mapped = warnings.iter().find(|w| w.contains("ANTHROPIC_API_KEY")).unwrap();
        assert!(
            mapped.contains("sbx secret set") && mapped.contains("anthropic"),
            "mapped key should mention its auto-registered service: {mapped}"
        );
        let unmapped = warnings.iter().find(|w| w.contains("MY_DB_PASSWORD")).unwrap();
        assert!(
            unmapped.contains("withheld"),
            "unmapped key should say it was withheld: {unmapped}"
        );
        assert!(
            !warnings.iter().any(|w| w.contains("LOG_LEVEL")),
            "non-credential keys must not be warned about"
        );
        assert!(
            !warnings.iter().any(|w| w.contains("hunter2")),
            "warnings must never include values"
        );
    }

    // ─── Argv construction ─────────────────────────────────────────────────

    #[test]
    fn first_launch_argv_has_kit_and_workspace() {
        let o = opts(vec![
            SandboxOption::AgentId("claude".into()),
            SandboxOption::WorkspaceDir("/wt".into()),
            SandboxOption::MemoryGb(8),
        ]);
        let argv = first_launch_argv("awman-h-claude", "claude", Path::new("/kits/claude"), &o);
        assert_eq!(argv[0], "run");
        assert!(argv.windows(2).any(|w| w[0] == "--kit" && w[1] == "/kits/claude"));
        assert!(argv.windows(2).any(|w| w[0] == "--name" && w[1] == "awman-h-claude"));
        assert!(argv.windows(2).any(|w| w[0] == "--workspace-dir" && w[1] == "/wt"));
        assert!(argv.windows(2).any(|w| w[0] == "--memory" && w[1] == "8g"));
        assert_eq!(argv.last().unwrap(), "claude");
    }

    #[test]
    fn restart_argv_omits_kit() {
        let o = opts(vec![SandboxOption::AgentId("claude".into())]);
        let argv = restart_argv("awman-h-claude", "claude", &o);
        assert_eq!(argv, vec!["run", "--name", "awman-h-claude", "claude"]);
        assert!(!argv.iter().any(|a| a == "--kit"));
    }

    #[test]
    fn seeded_prompt_positional_only_for_agent_kind() {
        // crush is a `kind: agent` kit → prompt appended positionally.
        let crush = opts(vec![
            SandboxOption::AgentId("crush".into()),
            SandboxOption::SeededPrompt("do the thing".into()),
        ]);
        let argv = restart_argv("awman-h-crush", "crush", &crush);
        assert_eq!(argv.last().unwrap(), "do the thing");

        // claude is a `kind: mixin` kit → prompt delivered via session.json.
        let claude = opts(vec![
            SandboxOption::AgentId("claude".into()),
            SandboxOption::SeededPrompt("do the thing".into()),
        ]);
        let argv = restart_argv("awman-h-claude", "claude", &claude);
        assert!(!argv.iter().any(|a| a == "do the thing"));
    }

    // ─── ls parsing ────────────────────────────────────────────────────────

    #[test]
    fn parse_ls_json_array_and_object() {
        let arr = r#"[{"name":"awman-h-claude"},{"name":"other"}]"#;
        assert_eq!(parse_ls_json(arr), vec!["awman-h-claude", "other"]);
        let obj = r#"{"sandboxes":[{"Name":"awman-x-codex"}]}"#;
        assert_eq!(parse_ls_json(obj), vec!["awman-x-codex"]);
    }

    #[test]
    fn parse_ls_table_skips_header_and_filters_prefix() {
        let table = "NAME            STATUS\nawman-h-claude  running\nnginx           running\n";
        assert_eq!(parse_ls_table(table), vec!["awman-h-claude"]);
    }

    #[test]
    fn parse_ls_json_empty_array() {
        assert_eq!(parse_ls_json("[]"), Vec::<String>::new());
    }

    #[test]
    fn parse_ls_json_invalid_is_empty() {
        assert_eq!(parse_ls_json("not json"), Vec::<String>::new());
    }

    #[test]
    fn parse_ls_table_empty_output_gives_empty_list() {
        assert_eq!(parse_ls_table(""), Vec::<String>::new());
    }

    // ─── Naming determinism ───────────────────────────────────────────────

    #[test]
    fn sandbox_name_same_inputs_same_output_multiple_calls() {
        let o = opts(vec![
            SandboxOption::AgentId("claude".into()),
            SandboxOption::WorkspaceDir("/projects/myapp".into()),
        ]);
        let n1 = resolve_sandbox_name(&o);
        let n2 = resolve_sandbox_name(&o);
        assert_eq!(n1, n2, "sandbox name must be deterministic");
    }

    #[test]
    fn sandbox_name_different_agents_differ() {
        let a = opts(vec![
            SandboxOption::AgentId("claude".into()),
            SandboxOption::WorkspaceDir("/wt".into()),
        ]);
        let b = opts(vec![
            SandboxOption::AgentId("gemini".into()),
            SandboxOption::WorkspaceDir("/wt".into()),
        ]);
        assert_ne!(
            resolve_sandbox_name(&a),
            resolve_sandbox_name(&b),
            "different agents must produce different sandbox names"
        );
    }

    #[test]
    fn sandbox_name_different_workspaces_differ() {
        let a = opts(vec![
            SandboxOption::AgentId("claude".into()),
            SandboxOption::WorkspaceDir("/wt/a".into()),
        ]);
        let b = opts(vec![
            SandboxOption::AgentId("claude".into()),
            SandboxOption::WorkspaceDir("/wt/b".into()),
        ]);
        assert_ne!(
            resolve_sandbox_name(&a),
            resolve_sandbox_name(&b),
            "different workspaces must produce different sandbox names"
        );
    }

    // ─── Argv — all variants ───────────────────────────────────────────────

    #[test]
    fn first_launch_argv_without_memory_omits_memory_flag() {
        let o = opts(vec![
            SandboxOption::AgentId("claude".into()),
            SandboxOption::WorkspaceDir("/wt".into()),
        ]);
        let argv = first_launch_argv("awman-h-claude", "claude", Path::new("/kits/claude"), &o);
        assert!(!argv.iter().any(|a| a == "--memory"), "no --memory when MemoryGb not set");
    }

    #[test]
    fn restart_argv_without_seeded_prompt_ends_with_agent() {
        let o = opts(vec![SandboxOption::AgentId("gemini".into())]);
        let argv = restart_argv("awman-h-gemini", "gemini", &o);
        assert_eq!(argv.last().unwrap(), "gemini");
        assert_eq!(argv.len(), 4); // ["run", "--name", "awman-h-gemini", "gemini"]
    }

    // Seeded prompts for mixin vs agent kit agents (full table check)
    #[test]
    fn all_mixin_agents_do_not_append_prompt_positionally() {
        let mixin_agents = ["claude", "codex", "gemini", "copilot", "opencode"];
        for agent in &mixin_agents {
            let o = opts(vec![
                SandboxOption::AgentId((*agent).into()),
                SandboxOption::SeededPrompt("the prompt".into()),
            ]);
            let argv = restart_argv("awman-h-x", agent, &o);
            assert!(
                !argv.iter().any(|a| a == "the prompt"),
                "mixin agent {agent}: seeded prompt must NOT be appended positionally"
            );
        }
    }

    #[test]
    fn all_agent_kit_agents_append_prompt_positionally() {
        let agent_kits = ["antigravity", "crush", "maki", "cline"];
        for agent in &agent_kits {
            let o = opts(vec![
                SandboxOption::AgentId((*agent).into()),
                SandboxOption::SeededPrompt("the prompt".into()),
            ]);
            let argv = restart_argv("awman-h-x", agent, &o);
            assert_eq!(
                argv.last().unwrap(),
                "the prompt",
                "agent-kit {agent}: seeded prompt must be appended positionally"
            );
        }
    }

    // ─── CPU limit warning via run_interactive ────────────────────────────
    //
    // `run_interactive` warns about CpuLimit before it tries to spawn `sbx`.
    // The warning must be in the sink even when `sbx` is not installed.
    // We use a thread-safe shared message log via Arc<Mutex<_>> so we can
    // inspect it after run_interactive takes ownership of the frontend.

    #[test]
    fn cpu_limit_produces_warning_in_run_interactive() {
        use crate::engine::agent_runtime::frontend::{
            AgentFrontend, AgentIo, AgentProgress, AgentStatus,
        };
        use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
        use std::sync::{Arc, Mutex};

        let messages: Arc<Mutex<Vec<UserMessage>>> = Arc::new(Mutex::new(Vec::new()));

        struct RecordingFrontend {
            messages: Arc<Mutex<Vec<UserMessage>>>,
        }
        impl UserMessageSink for RecordingFrontend {
            fn write_message(&mut self, msg: UserMessage) {
                self.messages.lock().unwrap().push(msg);
            }
            fn replay_queued(&mut self) {}
        }
        impl AgentFrontend for RecordingFrontend {
            fn report_status(&mut self, _: AgentStatus) {}
            fn report_progress(&mut self, _: AgentProgress) {}
            fn take_io(&mut self) -> AgentIo {
                let (stdout, _) = tokio::sync::mpsc::unbounded_channel();
                let (stderr, _) = tokio::sync::mpsc::unbounded_channel();
                let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
                AgentIo {
                    stdout,
                    stderr,
                    stdin_tx,
                    stdin_rx,
                    resize: None,
                    initial_size: None,
                }
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let frontend = Box::new(RecordingFrontend { messages: messages.clone() });
        let options = ResolvedSandboxOptions::resolve(vec![
            SandboxOption::AgentId("claude".into()),
            SandboxOption::WorkspaceDir(tmp.path().to_path_buf()),
            SandboxOption::CpuLimit(2.0),
        ]);
        // run_interactive will fail (sbx not installed) but the warning is
        // written before the spawn attempt.
        let _ = run_interactive(options, frontend);

        let msgs = messages.lock().unwrap();
        assert!(
            msgs.iter().any(|m| {
                m.level == MessageLevel::Warning
                    && m.text.contains("CPU")
                    && m.text.contains("not supported")
            }),
            "must produce a Warning about unsupported CPU limits; messages: {:?}",
            *msgs
        );
    }

    // ─── WI-0091: stdin seed routing (prompt delivered exactly once) ──────

    #[test]
    fn stdin_seed_present_for_mixin_kits_only() {
        let prompt = "fix the bug";
        for agent in ["claude", "codex", "gemini", "copilot", "opencode"] {
            let o = opts(vec![
                SandboxOption::AgentId(agent.into()),
                SandboxOption::SeededPrompt(prompt.into()),
            ]);
            assert_eq!(
                stdin_seed(agent, &o).as_deref(),
                Some(prompt),
                "mixin {agent}: stdin is the only delivery path, seed must be present"
            );
        }
        for agent in ["antigravity", "crush", "maki", "cline"] {
            let o = opts(vec![
                SandboxOption::AgentId(agent.into()),
                SandboxOption::SeededPrompt(prompt.into()),
            ]);
            assert_eq!(
                stdin_seed(agent, &o),
                None,
                "agent-kit {agent}: prompt is positional, stdin seed would deliver it twice"
            );
        }
    }

    #[test]
    fn stdin_seed_none_without_prompt() {
        let o = opts(vec![SandboxOption::AgentId("claude".into())]);
        assert_eq!(stdin_seed("claude", &o), None);
    }

    // ─── WI-0091: unsupported-feature notes surfaced as warnings ──────────

    #[test]
    fn unsupported_notes_produce_warnings_in_run_interactive() {
        use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
        use crate::engine::agent_runtime::frontend::{
            AgentFrontend, AgentIo, AgentProgress, AgentStatus,
        };
        use std::sync::{Arc, Mutex};

        let messages: Arc<Mutex<Vec<UserMessage>>> = Arc::new(Mutex::new(Vec::new()));

        struct RecordingFrontend {
            messages: Arc<Mutex<Vec<UserMessage>>>,
        }
        impl UserMessageSink for RecordingFrontend {
            fn write_message(&mut self, msg: UserMessage) {
                self.messages.lock().unwrap().push(msg);
            }
            fn replay_queued(&mut self) {}
        }
        impl AgentFrontend for RecordingFrontend {
            fn report_status(&mut self, _: AgentStatus) {}
            fn report_progress(&mut self, _: AgentProgress) {}
            fn take_io(&mut self) -> AgentIo {
                let (stdout, _) = tokio::sync::mpsc::unbounded_channel();
                let (stderr, _) = tokio::sync::mpsc::unbounded_channel();
                let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
                AgentIo {
                    stdout,
                    stderr,
                    stdin_tx,
                    stdin_rx,
                    resize: None,
                    initial_size: None,
                }
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let frontend = Box::new(RecordingFrontend { messages: messages.clone() });
        let options = ResolvedSandboxOptions::resolve(vec![
            SandboxOption::AgentId("claude".into()),
            SandboxOption::WorkspaceDir(tmp.path().to_path_buf()),
            SandboxOption::UnsupportedNote(
                "skill mounts are not supported under the sandbox runtime".into(),
            ),
        ]);
        // run_interactive will fail (sbx not installed) but the warning is
        // written before the spawn attempt.
        let _ = run_interactive(options, frontend);

        let msgs = messages.lock().unwrap();
        assert!(
            msgs.iter().any(|m| {
                m.level == MessageLevel::Warning && m.text.contains("skill mounts")
            }),
            "unsupported notes must surface as warnings before launch; messages: {:?}",
            *msgs
        );
    }
}
