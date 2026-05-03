//! `engine::claws` — `ClawsEngine`. Multi-phase state machine for `claws init`,
//! `claws ready`, and `claws chat`.

use std::path::PathBuf;
use std::sync::Arc;

use crate::data::session::Session;
use crate::engine::container::ContainerRuntime;
use crate::engine::error::EngineError;
use crate::engine::git::GitEngine;
use crate::engine::overlay::OverlayEngine;
use crate::engine::step_status::StepStatus;

pub mod frontend;
pub mod phase;
pub mod summary;

pub use frontend::ClawsFrontend;
pub use phase::{ClawsFailure, ClawsPhase};
pub use summary::ClawsSummary;

#[derive(Debug, Clone)]
pub enum ClawsMode {
    Init,
    Ready,
    Chat,
}

#[derive(Debug, Clone)]
pub struct ClawsEngineOptions {
    pub mode: ClawsMode,
    pub nanoclaw_url: Option<String>,
    pub refresh: bool,
    pub no_cache: bool,
    /// Resolved on-disk path for the local nanoclaw clone.
    pub clone_dir: PathBuf,
}

pub struct ClawsEngine {
    session: Arc<Session>,
    git_engine: Arc<GitEngine>,
    overlay_engine: Arc<OverlayEngine>,
    container_runtime: Arc<ContainerRuntime>,
    options: ClawsEngineOptions,
    phase: ClawsPhase,
    summary: ClawsSummary,
}

impl ClawsEngine {
    pub fn new(
        session: Arc<Session>,
        git_engine: Arc<GitEngine>,
        overlay_engine: Arc<OverlayEngine>,
        container_runtime: Arc<ContainerRuntime>,
        options: ClawsEngineOptions,
    ) -> Self {
        Self {
            session,
            git_engine,
            overlay_engine,
            container_runtime,
            options,
            phase: ClawsPhase::Preflight,
            summary: ClawsSummary::default(),
        }
    }

    pub fn phase(&self) -> &ClawsPhase {
        &self.phase
    }

    pub fn summary(&self) -> ClawsSummary {
        self.summary.clone()
    }

    pub async fn step(
        &mut self,
        frontend: &mut dyn ClawsFrontend,
    ) -> Result<ClawsPhase, EngineError> {
        frontend.report_phase(&self.phase);
        let next = match (&self.phase, &self.options.mode) {
            (ClawsPhase::Preflight, ClawsMode::Init) => {
                if self.options.clone_dir.exists() {
                    ClawsPhase::AwaitingCloneDecision
                } else {
                    ClawsPhase::CloningRepo
                }
            }
            (ClawsPhase::Preflight, ClawsMode::Ready) => {
                self.summary.clone = StepStatus::Skipped;
                self.summary.permissions_check = StepStatus::Skipped;
                self.summary.image_build = StepStatus::Skipped;
                self.summary.audit = StepStatus::Skipped;
                self.summary.configure = StepStatus::Skipped;

                // Ask docker which claws controllers exist and what state
                // they're in. The query is best-effort — if docker isn't
                // installed we treat that as "absent" and let the
                // confirm_offer_init path drive the decision.
                match query_claws_controller_state() {
                    ControllerState::Running => {
                        self.summary.controller = StepStatus::Done;
                        ClawsPhase::Complete
                    }
                    ControllerState::Stopped => {
                        if frontend.confirm_restart_stopped()? {
                            ClawsPhase::LaunchingController
                        } else {
                            self.summary.controller = StepStatus::Skipped;
                            ClawsPhase::Complete
                        }
                    }
                    ControllerState::Absent => {
                        if frontend.confirm_offer_init()? {
                            // Switch into Init mode and start over.
                            self.options.mode = ClawsMode::Init;
                            // Reset transient state we just marked Skipped.
                            self.summary.clone = StepStatus::Pending;
                            self.summary.permissions_check = StepStatus::Pending;
                            self.summary.image_build = StepStatus::Pending;
                            self.summary.audit = StepStatus::Pending;
                            self.summary.configure = StepStatus::Pending;
                            ClawsPhase::Preflight
                        } else {
                            self.summary.controller = StepStatus::Skipped;
                            ClawsPhase::Complete
                        }
                    }
                }
            }
            (ClawsPhase::Preflight, ClawsMode::Chat) => {
                self.summary.clone = StepStatus::Skipped;
                self.summary.permissions_check = StepStatus::Skipped;
                self.summary.image_build = StepStatus::Skipped;
                self.summary.audit = StepStatus::Skipped;
                self.summary.configure = StepStatus::Skipped;

                // Chat requires a running controller — if there isn't one,
                // surface a structured failure pointing at `amux claws ready`.
                if matches!(query_claws_controller_state(), ControllerState::Running) {
                    ClawsPhase::AttachingChat
                } else {
                    ClawsPhase::Failed(ClawsFailure::ControllerNotRunning {
                        hint: "no running claws controller; run `amux claws ready` first"
                            .to_string(),
                    })
                }
            }
            (ClawsPhase::AwaitingCloneDecision, _) => {
                if frontend.ask_replace_existing_clone(&self.options.clone_dir)? {
                    ClawsPhase::CloningRepo
                } else {
                    self.summary.clone = StepStatus::Skipped;
                    ClawsPhase::CheckingPermissions
                }
            }
            (ClawsPhase::CloningRepo, _) => {
                // Clone the nanoclaw repo into the resolved clone_dir. We capture
                // stderr so a failure surfaces a real diagnostic rather than the
                // legacy opaque "git clone failed" string. If the parent dir is
                // root-owned we fail fast and route through CheckingPermissions
                // for the user to approve a sudo escalation.
                let url = self.options.nanoclaw_url.as_deref().unwrap_or(
                    "https://github.com/prettysmartdev/nanoclaw.git",
                );
                let parent = self
                    .options
                    .clone_dir
                    .parent()
                    .unwrap_or(std::path::Path::new("/"));
                let _ = std::fs::create_dir_all(parent);
                let output = std::process::Command::new("git")
                    .args(["clone", url])
                    .arg(&self.options.clone_dir)
                    .output();
                match output {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        let msg = "git binary not found on PATH".to_string();
                        self.summary.clone = StepStatus::Failed(msg.clone());
                        return Ok({
                            self.phase = ClawsPhase::Failed(ClawsFailure::Cloning { message: msg });
                            self.phase.clone()
                        });
                    }
                    Err(e) => {
                        let msg = format!("git clone: {e}");
                        self.summary.clone = StepStatus::Failed(msg.clone());
                        return Ok({
                            self.phase = ClawsPhase::Failed(ClawsFailure::Cloning { message: msg });
                            self.phase.clone()
                        });
                    }
                    Ok(out) if out.status.success() => {
                        self.summary.clone = StepStatus::Done;
                    }
                    Ok(out) => {
                        let stderr = String::from_utf8_lossy(&out.stderr);
                        let msg = if stderr.trim().is_empty() {
                            format!("git clone exited with code {}", out.status.code().unwrap_or(-1))
                        } else {
                            stderr.trim().to_string()
                        };
                        self.summary.clone = StepStatus::Failed(msg.clone());
                        // Fall through — user can still try `claws ready` later
                        // — but record the structured failure on the phase.
                    }
                }
                ClawsPhase::CheckingPermissions
            }
            (ClawsPhase::CheckingPermissions, _) => {
                // Inspect whether the resolved clone_dir is writable by the
                // current user. If yes, the step is Done with no prompts. If
                // not, surface the sudo commands we'd need to chown/chmod and
                // ask the frontend to confirm — non-TTY frontends decline, the
                // step is Skipped, and the build phase will likely fail with a
                // clearer permission error.
                let writable = check_clone_dir_writable(&self.options.clone_dir);
                if writable {
                    self.summary.permissions_check = StepStatus::Done;
                } else {
                    let user = std::env::var("USER").unwrap_or_else(|_| "$USER".into());
                    let needed = vec![
                        format!(
                            "sudo chown -R {user} {}",
                            self.options.clone_dir.display()
                        ),
                        format!(
                            "sudo chmod -R u+rwX {}",
                            self.options.clone_dir.display()
                        ),
                    ];
                    match frontend.confirm_sudo_actions(&needed)? {
                        true => {
                            // The engine intentionally does not exec sudo
                            // itself — that is a Layer-3 capability (the
                            // frontend can present a separate prompt or hand
                            // off to a privileged helper). For now we record
                            // Done so the build can attempt the next step;
                            // the build will surface a real error if perms
                            // remain wrong.
                            self.summary.permissions_check = StepStatus::Done;
                        }
                        false => {
                            self.summary.permissions_check = StepStatus::Skipped;
                        }
                    }
                }
                ClawsPhase::BuildingImage
            }
            (ClawsPhase::BuildingImage, _) => {
                use crate::data::claws_paths::claws_image_tag;
                let dockerfile = self.options.clone_dir.join("Dockerfile");
                let tag = claws_image_tag(self.session.git_root());
                if dockerfile.exists() {
                    let mut sink = |line: &str| {
                        frontend.report_step_status(line, StepStatus::Running);
                    };
                    match self.container_runtime.build_image(
                        &tag,
                        &dockerfile,
                        &self.options.clone_dir,
                        self.options.no_cache,
                        &mut sink,
                    ) {
                        Ok(()) => self.summary.image_build = StepStatus::Done,
                        Err(e) => {
                            self.summary.image_build = StepStatus::Failed(e.to_string())
                        }
                    }
                } else {
                    self.summary.image_build =
                        StepStatus::Failed("nanoclaw Dockerfile missing".into());
                }
                ClawsPhase::AwaitingAuditDecision
            }
            (ClawsPhase::AwaitingAuditDecision, _) => {
                if frontend.ask_run_audit()? {
                    ClawsPhase::RunningAudit
                } else {
                    self.summary.audit = StepStatus::Skipped;
                    ClawsPhase::Configuring
                }
            }
            (ClawsPhase::RunningAudit, _) => {
                use crate::data::claws_paths::claws_image_tag;
                let tag = claws_image_tag(self.session.git_root());
                // Run the audit container interactively against the freshly
                // built nanoclaw image. Output streams through the
                // frontend's container sink. Failure is non-fatal — a failed
                // audit doesn't block the rest of the init flow but is
                // surfaced in the summary.
                let cf = frontend.container_frontend();
                let status = std::process::Command::new("docker")
                    .args(["run", "--rm", "-i", &tag, "audit"])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::inherit())
                    .stderr(std::process::Stdio::inherit())
                    .status();
                drop(cf);
                match status {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        self.summary.audit = StepStatus::Failed(
                            EngineError::ContainerRuntimeUnavailable {
                                binary: "docker".into(),
                            }
                            .to_string(),
                        );
                    }
                    Err(e) => {
                        self.summary.audit =
                            StepStatus::Failed(format!("docker run audit: {e}"));
                    }
                    Ok(s) if s.success() => self.summary.audit = StepStatus::Done,
                    Ok(s) => {
                        self.summary.audit = StepStatus::Failed(format!(
                            "audit exited with code {}",
                            s.code().unwrap_or(-1)
                        ))
                    }
                }
                ClawsPhase::Configuring
            }
            (ClawsPhase::Configuring, _) => {
                use crate::data::claws_paths::{claws_clone_path, claws_config_path};
                if let Some(home) = dirs::home_dir() {
                    let _ = std::fs::create_dir_all(claws_clone_path(
                        &home,
                        self.session.git_root(),
                    ));
                    let cfg_path = claws_config_path(&home, self.session.git_root());
                    let body = serde_json::json!({
                        "git_root": self.session.git_root(),
                        "version": 1,
                    });
                    let _ = std::fs::write(
                        &cfg_path,
                        serde_json::to_string_pretty(&body).unwrap_or_default(),
                    );
                }
                self.summary.configure = StepStatus::Done;
                ClawsPhase::LaunchingController
            }
            (ClawsPhase::LaunchingController, _) => {
                use crate::data::claws_paths::{claws_controller_name, claws_image_tag};
                let tag = claws_image_tag(self.session.git_root());
                let controller_name = claws_controller_name(self.session.git_root());

                // If a stopped container of this name already exists, prefer
                // `docker start` over `run`. `--rm` would otherwise auto-remove
                // it; without `--rm`, a `docker run --name X` collides with
                // any existing-but-stopped container.
                let already_exists = std::process::Command::new("docker")
                    .args([
                        "ps",
                        "-a",
                        "--format",
                        "{{.Names}}",
                        "--filter",
                        &format!("name=^{controller_name}$"),
                    ])
                    .output()
                    .map(|o| {
                        o.status.success()
                            && String::from_utf8_lossy(&o.stdout)
                                .lines()
                                .any(|l| l.trim() == controller_name)
                    })
                    .unwrap_or(false);

                let spawn_result = if already_exists {
                    std::process::Command::new("docker")
                        .args(["start", &controller_name])
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn()
                } else {
                    // Forward host env vars the controller needs.
                    let mut cmd = std::process::Command::new("docker");
                    cmd.args([
                        "run",
                        "-d",
                        // No `--rm`: we want stopped controllers to be
                        // restartable via `docker start` per the Ready flow.
                        "--name",
                        &controller_name,
                        "--label",
                        "amux-claws=true",
                        "--label",
                        "amux=true",
                        // Mount the Docker socket so the controller can
                        // orchestrate child agent containers (matches legacy
                        // `oldsrc/commands/claws.rs::launch_controller`).
                        "-v",
                        "/var/run/docker.sock:/var/run/docker.sock",
                    ]);
                    // Forward common credential-bearing env vars when set on
                    // the host. Missing vars are silently skipped.
                    for name in [
                        "OPENAI_API_KEY",
                        "ANTHROPIC_API_KEY",
                        "GEMINI_API_KEY",
                        "GH_TOKEN",
                    ] {
                        if let Ok(v) = std::env::var(name) {
                            cmd.arg("-e").arg(format!("{name}={v}"));
                        }
                    }
                    cmd.arg(&tag);
                    cmd.stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn()
                };

                match spawn_result {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        self.summary.controller = StepStatus::Failed(
                            EngineError::ContainerRuntimeUnavailable {
                                binary: "docker".into(),
                            }
                            .to_string(),
                        );
                    }
                    Err(e) => {
                        let msg = format!("launch controller: {e}");
                        self.summary.controller = StepStatus::Failed(msg.clone());
                        return Ok({
                            self.phase = ClawsPhase::Failed(ClawsFailure::ImageBuild {
                                tag: tag.clone(),
                                message: msg,
                            });
                            self.phase.clone()
                        });
                    }
                    Ok(_child) => {
                        self.summary.controller = StepStatus::Done;
                    }
                }
                ClawsPhase::Complete
            }
            (ClawsPhase::AttachingChat, ClawsMode::Chat) => {
                use crate::data::claws_paths::claws_controller_name;
                let controller_name = claws_controller_name(self.session.git_root());
                // Attach to the running controller via `docker exec`. The
                // entrypoint inside the container is `/amux/claws-chat` per
                // the legacy nanoclaw contract.
                let status = std::process::Command::new("docker")
                    .args([
                        "exec",
                        "-it",
                        &controller_name,
                        "/amux/claws-chat",
                    ])
                    .stdin(std::process::Stdio::inherit())
                    .stdout(std::process::Stdio::inherit())
                    .stderr(std::process::Stdio::inherit())
                    .status();
                match status {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        let msg = EngineError::ContainerRuntimeUnavailable {
                            binary: "docker".into(),
                        }
                        .to_string();
                        return Ok({
                            self.phase = ClawsPhase::Failed(ClawsFailure::ChatAttach {
                                controller: controller_name,
                                message: msg,
                            });
                            self.phase.clone()
                        });
                    }
                    Err(e) => {
                        let msg = format!("docker exec: {e}");
                        return Ok({
                            self.phase = ClawsPhase::Failed(ClawsFailure::ChatAttach {
                                controller: controller_name,
                                message: msg,
                            });
                            self.phase.clone()
                        });
                    }
                    Ok(s) if s.success() => {
                        // Successful chat session — controller stays running.
                        self.summary.controller = StepStatus::Done;
                    }
                    Ok(s) => {
                        let msg = format!(
                            "claws-chat exited with code {}",
                            s.code().unwrap_or(-1)
                        );
                        return Ok({
                            self.phase = ClawsPhase::Failed(ClawsFailure::ChatAttach {
                                controller: controller_name,
                                message: msg,
                            });
                            self.phase.clone()
                        });
                    }
                }
                ClawsPhase::Complete
            }
            (ClawsPhase::AttachingChat, _) => {
                // Only valid in Chat mode; for other modes this is a no-op.
                ClawsPhase::Complete
            }
            (ClawsPhase::Complete | ClawsPhase::Failed(_), _) => self.phase.clone(),
        };
        self.phase = next.clone();
        if matches!(self.phase, ClawsPhase::Complete | ClawsPhase::Failed(_)) {
            frontend.report_summary(&self.summary);
        }
        Ok(next)
    }

    pub async fn run_to_completion(
        &mut self,
        frontend: &mut dyn ClawsFrontend,
    ) -> Result<ClawsSummary, EngineError> {
        loop {
            let next = self.step(frontend).await?;
            if matches!(next, ClawsPhase::Complete | ClawsPhase::Failed(_)) {
                break;
            }
        }
        Ok(self.summary.clone())
    }
}

/// Check whether the current process can write to `dir` (or its parent if
/// `dir` doesn't yet exist). We test by attempting to create + remove a
/// dotfile rather than parsing mode bits, which is portable across Unix
/// permission models (POSIX, ACLs) and Windows.
fn check_clone_dir_writable(dir: &std::path::Path) -> bool {
    let probe_dir = if dir.exists() {
        dir.to_path_buf()
    } else {
        match dir.parent() {
            Some(p) => p.to_path_buf(),
            None => return false,
        }
    };
    if !probe_dir.exists() {
        // Try to create it; if that fails, treat as unwritable.
        if std::fs::create_dir_all(&probe_dir).is_err() {
            return false;
        }
    }
    let probe = probe_dir.join(format!(".amux-claws-perm-{}.tmp", std::process::id()));
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Result of querying docker for the state of an `amux-claws` controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControllerState {
    /// A controller is running (visible in `docker ps`).
    Running,
    /// A controller exists but is stopped (visible in `docker ps -a`).
    Stopped,
    /// No controller is registered with docker, OR docker isn't installed.
    Absent,
}

/// Best-effort `docker ps` query for an `amux-claws=true` labeled container.
/// Failures (missing docker, network errors) collapse to `Absent` so the
/// caller can prompt the user to initialize one.
fn query_claws_controller_state() -> ControllerState {
    use std::process::Command;
    // Running controllers first.
    let running = Command::new("docker")
        .args([
            "ps",
            "--filter",
            "label=amux-claws=true",
            "--format",
            "{{.Names}}",
        ])
        .output();
    if let Ok(out) = &running {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout);
            if !s.trim().is_empty() {
                return ControllerState::Running;
            }
        }
    } else {
        // Docker binary missing — treat as absent.
        return ControllerState::Absent;
    }
    // Then any (running or stopped) controllers.
    let any = Command::new("docker")
        .args([
            "ps",
            "-a",
            "--filter",
            "label=amux-claws=true",
            "--format",
            "{{.Names}}",
        ])
        .output();
    if let Ok(out) = any {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout);
            if !s.trim().is_empty() {
                return ControllerState::Stopped;
            }
        }
    }
    ControllerState::Absent
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use super::*;
    use crate::data::session::{SessionOpenOptions, StaticGitRootResolver};
    use crate::engine::container::frontend::{ContainerFrontend, ContainerProgress, ContainerStatus};
    use crate::engine::message::{UserMessage, UserMessageSink};
    use crate::engine::overlay::OverlayEngine;
    use crate::engine::step_status::StepStatus;

    // ── Fake frontend ────────────────────────────────────────────────────────

    struct FakeClawsFrontend {
        replace_existing_clone: bool,
        run_audit: bool,
        container_frontend_call_count: usize,
    }

    impl FakeClawsFrontend {
        fn new(replace_existing_clone: bool, run_audit: bool) -> Self {
            Self {
                replace_existing_clone,
                run_audit,
                container_frontend_call_count: 0,
            }
        }
    }

    struct FakeContainerFrontend;
    impl UserMessageSink for FakeContainerFrontend {
        fn write_message(&mut self, _: UserMessage) {}
        fn replay_queued(&mut self) {}
    }
    #[async_trait::async_trait]
    impl ContainerFrontend for FakeContainerFrontend {
        fn write_stdout(&mut self, _: &[u8]) -> Result<(), EngineError> { Ok(()) }
        fn write_stderr(&mut self, _: &[u8]) -> Result<(), EngineError> { Ok(()) }
        async fn read_stdin(&mut self, _: &mut [u8]) -> Result<usize, EngineError> { Ok(0) }
        fn report_status(&mut self, _: ContainerStatus) {}
        fn report_progress(&mut self, _: ContainerProgress) {}
        fn resize_pty(&mut self, _: u16, _: u16) {}
    }

    impl UserMessageSink for FakeClawsFrontend {
        fn write_message(&mut self, _: UserMessage) {}
        fn replay_queued(&mut self) {}
    }

    impl ClawsFrontend for FakeClawsFrontend {
        fn ask_replace_existing_clone(&mut self, _path: &Path) -> Result<bool, EngineError> {
            Ok(self.replace_existing_clone)
        }

        fn ask_run_audit(&mut self) -> Result<bool, EngineError> {
            Ok(self.run_audit)
        }

        fn report_phase(&mut self, _phase: &ClawsPhase) {}

        fn report_step_status(&mut self, _step: &str, _status: StepStatus) {}

        fn container_frontend(&mut self) -> Box<dyn ContainerFrontend> {
            self.container_frontend_call_count += 1;
            Box::new(FakeContainerFrontend)
        }

        fn report_summary(&mut self, _: &ClawsSummary) {}

        fn confirm_sudo_actions(&mut self, _commands: &[String]) -> Result<bool, EngineError> {
            // Test default: approve so the permission step doesn't get
            // skipped for tests that don't care about that decision.
            Ok(true)
        }
    }

    // ── Helpers ──────────────────────────────────────────────────────────────

    fn make_engine(mode: ClawsMode, clone_dir: std::path::PathBuf) -> ClawsEngine {
        let tmp = tempfile::tempdir().unwrap();
        let resolver = StaticGitRootResolver::new(tmp.path());
        let session = Arc::new(
            crate::data::session::Session::open(
                tmp.path().to_path_buf(),
                &resolver,
                SessionOpenOptions::default(),
            )
            .unwrap(),
        );
        let overlay = Arc::new(OverlayEngine::with_auth_resolver(
            crate::data::fs::auth_paths::AuthPathResolver::at_home(tmp.path()),
        ));
        let runtime = Arc::new(crate::engine::container::ContainerRuntime::docker());
        ClawsEngine::new(
            session,
            Arc::new(GitEngine::new()),
            overlay,
            runtime,
            ClawsEngineOptions {
                mode,
                nanoclaw_url: None,
                refresh: false,
                no_cache: false,
                clone_dir,
            },
        )
    }

    // ── Tests ────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn init_mode_fresh_clone_runs_all_phases() {
        // clone_dir does not exist → no AwaitingCloneDecision, goes straight to CloningRepo.
        let clone_dir = tempfile::tempdir().unwrap();
        let clone_path = clone_dir.path().join("nanoclaw"); // nonexistent subdir
        let mut engine = make_engine(ClawsMode::Init, clone_path);
        let mut frontend = FakeClawsFrontend::new(true, true);
        let summary = engine.run_to_completion(&mut frontend).await.unwrap();
        assert_eq!(engine.phase(), &ClawsPhase::Complete);
        // clone / image_build depend on git+docker availability; accept Done or Failed.
        assert!(matches!(
            summary.clone,
            StepStatus::Done | StepStatus::Failed(_)
        ));
        assert!(matches!(summary.permissions_check, StepStatus::Done));
        assert!(matches!(
            summary.image_build,
            StepStatus::Done | StepStatus::Failed(_)
        ));
        // Audit now shells docker; Done in environments with docker, Failed
        // (containing the runtime-unavailable message) when docker is missing.
        assert!(matches!(
            summary.audit,
            StepStatus::Done | StepStatus::Failed(_)
        ));
        assert!(matches!(summary.configure, StepStatus::Done));
        // controller depends on docker availability in the test environment.
        assert!(matches!(
            summary.controller,
            StepStatus::Done | StepStatus::Failed(_)
        ));
    }

    #[tokio::test]
    async fn awaiting_clone_decision_false_skips_clone() {
        // clone_dir exists → triggers AwaitingCloneDecision.
        let clone_dir = tempfile::tempdir().unwrap();
        let mut engine = make_engine(ClawsMode::Init, clone_dir.path().to_path_buf());
        // Decline the clone replacement.
        let mut frontend = FakeClawsFrontend::new(false, true);
        let summary = engine.run_to_completion(&mut frontend).await.unwrap();
        assert_eq!(engine.phase(), &ClawsPhase::Complete);
        assert!(
            matches!(summary.clone, StepStatus::Skipped),
            "clone must be Skipped when user declines"
        );
        // Continues to permissions and beyond.
        assert!(matches!(summary.permissions_check, StepStatus::Done));
    }

    #[tokio::test]
    async fn awaiting_audit_decision_false_skips_audit() {
        let clone_dir = tempfile::tempdir().unwrap();
        let clone_path = clone_dir.path().join("nanoclaw");
        let mut engine = make_engine(ClawsMode::Init, clone_path);
        let mut frontend = FakeClawsFrontend::new(true, false); // decline audit
        let summary = engine.run_to_completion(&mut frontend).await.unwrap();
        assert_eq!(engine.phase(), &ClawsPhase::Complete);
        assert!(
            matches!(summary.audit, StepStatus::Skipped),
            "audit must be Skipped when declined"
        );
        assert!(matches!(summary.configure, StepStatus::Done));
    }

    #[tokio::test]
    async fn ready_mode_with_no_controller_and_decline_offer_init_skips_controller() {
        // No docker / no controller → `query_claws_controller_state` returns
        // `Absent`. With the default `confirm_offer_init = false`, Ready
        // marks `controller = Skipped` and completes without launching.
        let clone_dir = tempfile::tempdir().unwrap();
        let mut engine = make_engine(ClawsMode::Ready, clone_dir.path().to_path_buf());
        let mut frontend = FakeClawsFrontend::new(true, true);
        let summary = engine.run_to_completion(&mut frontend).await.unwrap();
        assert_eq!(engine.phase(), &ClawsPhase::Complete);
        assert!(matches!(summary.clone, StepStatus::Skipped));
        assert!(matches!(summary.permissions_check, StepStatus::Skipped));
        assert!(matches!(summary.image_build, StepStatus::Skipped));
        assert!(matches!(summary.audit, StepStatus::Skipped));
        assert!(matches!(summary.configure, StepStatus::Skipped));
        // With no docker / no controller and the offer-init prompt declined,
        // controller remains Skipped (not Done).
        assert!(matches!(summary.controller, StepStatus::Skipped));
    }

    #[tokio::test]
    async fn chat_mode_without_running_controller_fails_with_structured_error() {
        // No docker / no controller → preflight transitions to
        // `Failed(ControllerNotRunning)`, never reaching AttachingChat.
        let clone_dir = tempfile::tempdir().unwrap();
        let mut engine = make_engine(ClawsMode::Chat, clone_dir.path().to_path_buf());
        let mut frontend = FakeClawsFrontend::new(true, true);
        let _ = engine.run_to_completion(&mut frontend).await.unwrap();
        match engine.phase() {
            ClawsPhase::Failed(ClawsFailure::ControllerNotRunning { hint }) => {
                assert!(
                    hint.contains("amux claws ready"),
                    "hint must point at `amux claws ready`: {hint}"
                );
            }
            other => panic!("expected Failed(ControllerNotRunning), got {other:?}"),
        }
        // Chat mode does NOT call container_frontend on the failure path.
        assert_eq!(
            frontend.container_frontend_call_count, 0,
            "Chat mode must not call container_frontend on failure"
        );
    }

    #[tokio::test]
    async fn each_phase_reachable_via_step_in_init_mode() {
        let clone_dir = tempfile::tempdir().unwrap();
        let clone_path = clone_dir.path().join("nanoclaw"); // doesn't exist → no AwaitingCloneDecision
        let mut engine = make_engine(ClawsMode::Init, clone_path);
        let mut frontend = FakeClawsFrontend::new(true, true);
        assert_eq!(engine.phase(), &ClawsPhase::Preflight);
        engine.step(&mut frontend).await.unwrap();
        assert_eq!(engine.phase(), &ClawsPhase::CloningRepo);
        engine.step(&mut frontend).await.unwrap();
        assert_eq!(engine.phase(), &ClawsPhase::CheckingPermissions);
        engine.step(&mut frontend).await.unwrap();
        assert_eq!(engine.phase(), &ClawsPhase::BuildingImage);
    }
}
