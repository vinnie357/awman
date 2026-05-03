//! `engine::init` — `InitEngine`. Multi-phase state machine for `amux init`.

use std::path::PathBuf;
use std::sync::Arc;

use crate::data::session::{AgentName, Session};
use crate::engine::agent::AgentEngine;
use crate::engine::container::ContainerRuntime;
use crate::engine::error::EngineError;
use crate::engine::git::GitEngine;
use crate::engine::overlay::OverlayEngine;
use crate::engine::step_status::StepStatus;

pub mod frontend;
pub mod phase;
pub mod summary;

pub use frontend::InitFrontend;
pub use phase::{InitFailure, InitPhase};
pub use summary::InitSummary;

#[derive(Debug, Clone)]
pub struct InitEngineOptions {
    pub agent: AgentName,
    pub run_aspec_setup: bool,
    pub git_root: PathBuf,
}

pub struct InitEngine {
    session: Arc<Session>,
    git_engine: Arc<GitEngine>,
    overlay_engine: Arc<OverlayEngine>,
    container_runtime: Arc<ContainerRuntime>,
    agent_engine: Arc<AgentEngine>,
    options: InitEngineOptions,
    phase: InitPhase,
    summary: InitSummary,
}

impl InitEngine {
    pub fn new(
        session: Arc<Session>,
        git_engine: Arc<GitEngine>,
        overlay_engine: Arc<OverlayEngine>,
        container_runtime: Arc<ContainerRuntime>,
        agent_engine: Arc<AgentEngine>,
        options: InitEngineOptions,
    ) -> Self {
        Self {
            session,
            git_engine,
            overlay_engine,
            container_runtime,
            agent_engine,
            options,
            phase: InitPhase::Preflight,
            summary: InitSummary::default(),
        }
    }

    pub fn phase(&self) -> &InitPhase {
        &self.phase
    }

    pub fn summary(&self) -> &InitSummary {
        &self.summary
    }

    pub async fn step(
        &mut self,
        frontend: &mut dyn InitFrontend,
    ) -> Result<InitPhase, EngineError> {
        use crate::data::config::repo::RepoConfig;
        use crate::data::image_tags::project_image_tag;
        use crate::data::repo_dockerfile_paths::RepoDockerfilePaths;
        use crate::data::templates;

        frontend.report_phase(&self.phase);
        let git_root = self.options.git_root.clone();

        let next = match &self.phase {
            InitPhase::Preflight => {
                let _ = self.git_engine;
                let _ = self.overlay_engine;
                InitPhase::AwaitingAspecDecision
            }
            InitPhase::AwaitingAspecDecision => {
                if frontend.ask_replace_aspec()? {
                    InitPhase::CreatingAspecFolder
                } else {
                    self.summary.aspec_folder = StepStatus::Skipped;
                    InitPhase::SettingUpDockerfile
                }
            }
            InitPhase::CreatingAspecFolder => {
                let aspec_dir = git_root.join("aspec");
                let mut downloaded = false;
                if self.options.run_aspec_setup {
                    match crate::data::network::download_aspec_tarball().await {
                        Ok(bytes) => {
                            match crate::data::network::extract_aspec_tarball(&bytes, &aspec_dir) {
                                Ok(()) => downloaded = true,
                                Err(e) => {
                                    frontend.write_message(crate::engine::message::UserMessage {
                                        level: crate::engine::message::MessageLevel::Warning,
                                        text: format!("aspec download failed: {e}; using empty aspec directory"),
                                    });
                                }
                            }
                        }
                        Err(e) => {
                            frontend.write_message(crate::engine::message::UserMessage {
                                level: crate::engine::message::MessageLevel::Warning,
                                text: format!("aspec download failed: {e}; using empty aspec directory"),
                            });
                        }
                    }
                }
                if !downloaded {
                    // Fall back to creating an empty aspec dir so subsequent
                    // engines can write into it.
                    if !aspec_dir.exists() {
                        std::fs::create_dir_all(&aspec_dir)
                            .map_err(|e| EngineError::io(aspec_dir.clone(), e))?;
                    }
                }
                self.summary.aspec_folder = StepStatus::Done;
                InitPhase::SettingUpDockerfile
            }
            InitPhase::SettingUpDockerfile => {
                let paths = RepoDockerfilePaths::new(&git_root);
                let dockerfile_path = paths.project_dockerfile();
                if !dockerfile_path.exists() {
                    std::fs::write(&dockerfile_path, templates::project_dockerfile_dev())
                        .map_err(|e| EngineError::io(dockerfile_path.clone(), e))?;
                }
                self.summary.dockerfile = StepStatus::Done;
                // Issue 10: Next phase creates the agent Dockerfile.
                InitPhase::SettingUpAgentDockerfile
            }
            // Issue 10: Ensure .amux/Dockerfile.<agent> exists.
            InitPhase::SettingUpAgentDockerfile => {
                let paths = RepoDockerfilePaths::new(&git_root);
                let agent_dockerfile = paths.agent_dockerfile(self.options.agent.as_str());
                let project_tag = crate::data::image_tags::project_image_tag(&git_root);
                if !agent_dockerfile.exists() {
                    let dl = crate::engine::agent::download::download_agent_dockerfile(
                        self.options.agent.as_str(),
                        &agent_dockerfile,
                        &project_tag,
                    )
                    .await;
                    if let Err(e) = dl {
                        frontend.write_message(crate::engine::message::UserMessage {
                            level: crate::engine::message::MessageLevel::Warning,
                            text: format!("agent Dockerfile download failed: {e}; continuing without it"),
                        });
                    }
                }
                InitPhase::WritingConfig
            }
            InitPhase::WritingConfig => {
                let config_path = RepoConfig::path(&git_root);
                if !config_path.exists() {
                    let cfg = RepoConfig {
                        agent: Some(self.options.agent.as_str().to_string()),
                        ..Default::default()
                    };
                    cfg.save(&git_root)?;
                } else {
                    frontend.write_message(crate::engine::message::UserMessage {
                        level: crate::engine::message::MessageLevel::Info,
                        text: "aspec/.amux.json already present — preserving existing config."
                            .to_string(),
                    });
                }
                self.summary.config = StepStatus::Done;
                InitPhase::AwaitingAuditDecision
            }
            InitPhase::AwaitingAuditDecision => {
                if frontend.ask_run_audit()? {
                    InitPhase::BuildingImage
                } else {
                    self.summary.audit = StepStatus::Skipped;
                    self.summary.image_build = StepStatus::Skipped;
                    self.summary.agent_image_build = StepStatus::Skipped;
                    self.summary.image_rebuild = StepStatus::Skipped;
                    InitPhase::AwaitingWorkItemsDecision
                }
            }
            InitPhase::BuildingImage => {
                // Issue 16: Docker daemon pre-check — soft failure allows
                // run_to_completion to surface a summary rather than aborting.
                if !self.container_runtime.is_available() {
                    let msg = "Docker daemon is not running. Install Docker and retry.".to_string();
                    self.summary.image_build = StepStatus::Failed(msg.clone());
                    self.summary.audit = StepStatus::Skipped;
                    self.summary.agent_image_build = StepStatus::Skipped;
                    self.summary.image_rebuild = StepStatus::Skipped;
                    frontend.report_step_status("Build image", StepStatus::Failed(msg));
                    return Ok(InitPhase::AwaitingWorkItemsDecision);
                }

                let paths = RepoDockerfilePaths::new(&git_root);
                let dockerfile_path = paths.project_dockerfile();
                let tag = project_image_tag(&git_root);
                frontend.report_step_status("Build base image", StepStatus::Running);
                let mut sink = |line: &str| {
                    frontend.report_step_status(line, StepStatus::Running);
                };
                let result = self.container_runtime.build_image(
                    &tag,
                    &dockerfile_path,
                    &git_root,
                    false,
                    &mut sink,
                );
                match result {
                    Ok(()) => {
                        self.summary.image_build = StepStatus::Done;
                        frontend.report_step_status("Build base image", StepStatus::Done);
                        // Issue 11: Next phase builds the agent image.
                        InitPhase::BuildingAgentImage
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        self.summary.image_build = StepStatus::Failed(msg.clone());
                        frontend
                            .report_step_status("Build base image", StepStatus::Failed(msg.clone()));
                        // Skip audit; nothing to audit without a base image.
                        self.summary.audit = StepStatus::Skipped;
                        self.summary.agent_image_build = StepStatus::Skipped;
                        self.summary.image_rebuild = StepStatus::Skipped;
                        InitPhase::AwaitingWorkItemsDecision
                    }
                }
            }
            // Issue 11: Build the agent image after the base image.
            InitPhase::BuildingAgentImage => {
                use crate::data::image_tags::agent_image_tag;

                let paths = RepoDockerfilePaths::new(&git_root);
                let agent_dockerfile = paths.agent_dockerfile(self.options.agent.as_str());
                let agent_tag = agent_image_tag(&git_root, self.options.agent.as_str());

                if agent_dockerfile.exists() {
                    frontend.report_step_status("Build agent image", StepStatus::Running);
                    let mut sink = |line: &str| {
                        frontend.report_step_status(line, StepStatus::Running);
                    };
                    let result = self.container_runtime.build_image(
                        &agent_tag,
                        &agent_dockerfile,
                        &git_root,
                        false,
                        &mut sink,
                    );
                    match result {
                        Ok(()) => {
                            self.summary.agent_image_build = StepStatus::Done;
                            frontend.report_step_status("Build agent image", StepStatus::Done);
                        }
                        Err(e) => {
                            let msg = e.to_string();
                            self.summary.agent_image_build = StepStatus::Failed(msg.clone());
                            frontend.report_step_status("Build agent image", StepStatus::Failed(msg));
                        }
                    }
                } else {
                    self.summary.agent_image_build = StepStatus::Skipped;
                    frontend.write_message(crate::engine::message::UserMessage {
                        level: crate::engine::message::MessageLevel::Warning,
                        text: "Agent Dockerfile not found; skipping agent image build.".to_string(),
                    });
                }
                InitPhase::RunningAudit
            }
            InitPhase::RunningAudit => {
                use crate::data::templates::init_audit_prompt;
                use crate::engine::agent::AgentRunOptions;

                // Route through `AgentEngine::build_options` so overlays,
                // agent settings, env passthrough, and the standard /workspace
                // working dir all apply — matching `ReadyEngine::RunningAudit`.
                let run_opts = AgentRunOptions {
                    yolo: None,
                    auto: None,
                    plan: None,
                    allowed_tools: vec![],
                    disallowed_tools: vec![],
                    initial_prompt: Some(init_audit_prompt().to_string()),
                    allow_docker: false,
                    mount_ssh: false,
                    non_interactive: true,
                    model: None,
                    env_passthrough: None,
                    directory_overlays: vec![],
                };
                match self
                    .agent_engine
                    .build_options(&self.session, &self.options.agent, &run_opts)
                {
                    Err(e) => {
                        // Unknown agent or option-build failure — skip audit
                        // gracefully (init flow continues).
                        self.summary.audit = StepStatus::Skipped;
                        frontend.write_message(crate::engine::message::UserMessage {
                            level: crate::engine::message::MessageLevel::Warning,
                            text: format!("skipping audit: {e}"),
                        });
                    }
                    Ok(options) => {
                        match self.container_runtime.build(options) {
                            Err(e) => {
                                self.summary.audit = StepStatus::Skipped;
                                frontend.write_message(crate::engine::message::UserMessage {
                                    level: crate::engine::message::MessageLevel::Warning,
                                    text: format!("skipping audit: {e}"),
                                });
                            }
                            Ok(instance) => {
                                let container_fe = frontend.container_frontend();
                                match instance.run_with_frontend(container_fe) {
                                    Err(e) => {
                                        self.summary.audit = StepStatus::Skipped;
                                        frontend.write_message(crate::engine::message::UserMessage {
                                            level: crate::engine::message::MessageLevel::Warning,
                                            text: format!("skipping audit: {e}"),
                                        });
                                    }
                                    Ok(mut exec) => match exec.wait().await {
                                        Err(e) => {
                                            self.summary.audit = StepStatus::Failed(e.to_string());
                                        }
                                        Ok(exit) => {
                                            if exit.exit_code == 0 {
                                                self.summary.audit = StepStatus::Done;
                                            } else {
                                                self.summary.audit = StepStatus::Failed(
                                                    format!("audit exited with code {}", exit.exit_code),
                                                );
                                            }
                                        }
                                    },
                                }
                            }
                        }
                    }
                }
                // Issue 12: After the audit, rebuild images if audit succeeded.
                InitPhase::RebuildingAfterAudit
            }
            // Issue 12: Post-audit image rebuild in init.
            InitPhase::RebuildingAfterAudit => {
                if matches!(self.summary.audit, StepStatus::Done) {
                    // Rebuild base image.
                    let paths = RepoDockerfilePaths::new(&git_root);
                    let dockerfile_path = paths.project_dockerfile();
                    let tag = project_image_tag(&git_root);
                    frontend.report_step_status("Rebuilding after audit", StepStatus::Running);
                    let mut sink = |line: &str| {
                        frontend.report_step_status(line, StepStatus::Running);
                    };
                    let result = self.container_runtime.build_image(
                        &tag,
                        &dockerfile_path,
                        &git_root,
                        false,
                        &mut sink,
                    );
                    match result {
                        Ok(()) => {
                            self.summary.image_rebuild = StepStatus::Done;
                            frontend.report_step_status("Rebuilding after audit", StepStatus::Done);
                        }
                        Err(e) => {
                            let msg = e.to_string();
                            self.summary.image_rebuild = StepStatus::Failed(msg.clone());
                            frontend.report_step_status(
                                "Rebuilding after audit",
                                StepStatus::Failed(msg),
                            );
                        }
                    }
                    // Also rebuild agent image.
                    if matches!(self.summary.image_rebuild, StepStatus::Done) {
                        use crate::data::image_tags::agent_image_tag;
                        let agent_dockerfile = paths.agent_dockerfile(self.options.agent.as_str());
                        if agent_dockerfile.exists() {
                            let agent_tag = agent_image_tag(&git_root, self.options.agent.as_str());
                            let mut agent_sink = |line: &str| {
                                frontend.report_step_status(line, StepStatus::Running);
                            };
                            let _ = self.container_runtime.build_image(
                                &agent_tag,
                                &agent_dockerfile,
                                &git_root,
                                false,
                                &mut agent_sink,
                            );
                        }
                    }
                } else {
                    self.summary.image_rebuild = StepStatus::Skipped;
                }
                InitPhase::AwaitingWorkItemsDecision
            }
            InitPhase::AwaitingWorkItemsDecision => {
                let cfg = frontend.ask_work_items_setup()?;
                if let Some(work_items) = cfg {
                    let mut repo_cfg = RepoConfig::load(&git_root)?;
                    repo_cfg.set_work_items_config(Some(work_items));
                    repo_cfg.save(&git_root)?;
                    InitPhase::WritingWorkItemsConfig
                } else {
                    self.summary.work_items_setup = StepStatus::Skipped;
                    InitPhase::Complete
                }
            }
            InitPhase::WritingWorkItemsConfig => {
                self.summary.work_items_setup = StepStatus::Done;
                InitPhase::Complete
            }
            InitPhase::Complete | InitPhase::Failed(_) => self.phase.clone(),
        };
        self.phase = next.clone();
        if matches!(self.phase, InitPhase::Complete | InitPhase::Failed(_)) {
            frontend.report_summary(&self.summary);
        }
        Ok(next)
    }

    pub async fn run_to_completion(
        &mut self,
        frontend: &mut dyn InitFrontend,
    ) -> Result<InitSummary, EngineError> {
        loop {
            let next = self.step(frontend).await?;
            if matches!(next, InitPhase::Complete | InitPhase::Failed(_)) {
                break;
            }
        }
        Ok(self.summary.clone())
    }
}


#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::data::config::repo::WorkItemsConfig;
    use crate::data::session::{SessionOpenOptions, StaticGitRootResolver};
    use crate::engine::container::frontend::{ContainerFrontend, ContainerProgress, ContainerStatus};
    use crate::engine::message::{UserMessage, UserMessageSink};
    use crate::engine::overlay::OverlayEngine;
    use crate::engine::step_status::StepStatus;

    // -- Fake frontend --------------------------------------------------------

    struct FakeInitFrontend {
        replace_aspec: bool,
        run_audit: bool,
        work_items_config: Option<WorkItemsConfig>,
        phases: Vec<InitPhase>,
    }

    impl FakeInitFrontend {
        fn all_yes() -> Self {
            Self {
                replace_aspec: true,
                run_audit: true,
                work_items_config: Some(WorkItemsConfig::default()),
                phases: Vec::new(),
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

    impl UserMessageSink for FakeInitFrontend {
        fn write_message(&mut self, _: UserMessage) {}
        fn replay_queued(&mut self) {}
    }

    impl InitFrontend for FakeInitFrontend {
        fn ask_replace_aspec(&mut self) -> Result<bool, EngineError> {
            Ok(self.replace_aspec)
        }

        fn ask_run_audit(&mut self) -> Result<bool, EngineError> {
            Ok(self.run_audit)
        }

        fn ask_work_items_setup(&mut self) -> Result<Option<WorkItemsConfig>, EngineError> {
            Ok(self.work_items_config.clone())
        }

        fn report_phase(&mut self, phase: &InitPhase) {
            self.phases.push(phase.clone());
        }

        fn report_step_status(&mut self, _step: &str, _status: StepStatus) {}

        fn container_frontend(&mut self) -> Box<dyn ContainerFrontend> {
            Box::new(FakeContainerFrontend)
        }

        fn report_summary(&mut self, _: &InitSummary) {}
    }

    // -- Helpers --------------------------------------------------------------

    fn make_engine(git_root: &std::path::Path) -> InitEngine {
        // Pre-create agent Dockerfile so the engine does not attempt a network
        // download during tests.
        let amux_dir = git_root.join(".amux");
        let _ = std::fs::create_dir_all(&amux_dir);
        let _ = std::fs::write(amux_dir.join("Dockerfile.claude"), "FROM scratch\n");
        let resolver = StaticGitRootResolver::new(git_root);
        let session = Arc::new(
            crate::data::session::Session::open(
                git_root.to_path_buf(),
                &resolver,
                SessionOpenOptions::default(),
            )
            .unwrap(),
        );
        let overlay = Arc::new(OverlayEngine::with_auth_resolver(
            crate::data::fs::auth_paths::AuthPathResolver::at_home(git_root),
        ));
        let runtime = Arc::new(crate::engine::container::ContainerRuntime::docker());
        let agent_engine = Arc::new(crate::engine::agent::AgentEngine::new(
            Arc::clone(&overlay),
            Arc::clone(&runtime),
        ));
        let options = InitEngineOptions {
            agent: AgentName::new("claude").unwrap(),
            run_aspec_setup: false,
            git_root: git_root.to_path_buf(),
        };
        InitEngine::new(
            session,
            Arc::new(GitEngine::new()),
            overlay,
            runtime,
            agent_engine,
            options,
        )
    }

    // -- Tests ----------------------------------------------------------------

    #[tokio::test]
    async fn run_to_completion_all_done() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = make_engine(tmp.path());
        let mut frontend = FakeInitFrontend::all_yes();
        let summary = engine.run_to_completion(&mut frontend).await.unwrap();
        assert_eq!(engine.phase(), &InitPhase::Complete);
        // The aspec download will fail with no network and fall back to the
        // bundled aspec dir; structurally it lands at Done.
        assert!(matches!(summary.aspec_folder, StepStatus::Done));
        assert!(matches!(summary.dockerfile, StepStatus::Done));
        assert!(matches!(summary.config, StepStatus::Done));
        // image_build may be Done, Skipped, or Failed depending on whether
        // docker is available in the test environment.
        assert!(matches!(
            summary.image_build,
            StepStatus::Done | StepStatus::Skipped | StepStatus::Failed(_)
        ));
        // The audit only runs when image_build succeeds.
        assert!(matches!(
            summary.audit,
            StepStatus::Done | StepStatus::Skipped
        ));
        assert!(matches!(summary.work_items_setup, StepStatus::Done));
    }

    #[tokio::test]
    async fn awaiting_aspec_decision_false_skips_aspec_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = make_engine(tmp.path());
        let mut frontend = FakeInitFrontend {
            replace_aspec: false,
            run_audit: true,
            work_items_config: Some(WorkItemsConfig::default()),
            phases: Vec::new(),
        };
        let summary = engine.run_to_completion(&mut frontend).await.unwrap();
        assert_eq!(engine.phase(), &InitPhase::Complete);
        assert!(
            matches!(summary.aspec_folder, StepStatus::Skipped),
            "aspec_folder must be Skipped when user declines"
        );
        // Other phases continue.
        assert!(matches!(summary.dockerfile, StepStatus::Done));
    }

    #[tokio::test]
    async fn awaiting_work_items_decision_none_skips_work_items() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = make_engine(tmp.path());
        let mut frontend = FakeInitFrontend {
            replace_aspec: true,
            run_audit: true,
            work_items_config: None, // decline work-items setup
            phases: Vec::new(),
        };
        let summary = engine.run_to_completion(&mut frontend).await.unwrap();
        assert_eq!(engine.phase(), &InitPhase::Complete);
        assert!(
            matches!(summary.work_items_setup, StepStatus::Skipped),
            "work_items_setup must be Skipped when None returned"
        );
    }

    #[tokio::test]
    async fn each_phase_independently_reachable_via_step() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = make_engine(tmp.path());
        let mut frontend = FakeInitFrontend::all_yes();
        assert_eq!(engine.phase(), &InitPhase::Preflight);
        engine.step(&mut frontend).await.unwrap();
        assert_eq!(engine.phase(), &InitPhase::AwaitingAspecDecision);
        engine.step(&mut frontend).await.unwrap();
        assert_eq!(engine.phase(), &InitPhase::CreatingAspecFolder);
        engine.step(&mut frontend).await.unwrap();
        assert_eq!(engine.phase(), &InitPhase::SettingUpDockerfile);
    }

    #[tokio::test]
    async fn writing_config_creates_config_file() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = make_engine(tmp.path());
        let mut frontend = FakeInitFrontend {
            replace_aspec: true,
            run_audit: false,
            work_items_config: None,
            phases: Vec::new(),
        };
        let summary = engine.run_to_completion(&mut frontend).await.unwrap();
        assert!(matches!(summary.config, StepStatus::Done));
        // Config file must exist after WritingConfig phase.
        let config_path = crate::data::config::repo::RepoConfig::path(tmp.path());
        assert!(
            config_path.exists(),
            "WritingConfig phase must create the repo config file"
        );
    }

    #[tokio::test]
    async fn writing_config_is_idempotent() {
        // Running init twice on the same repo must not corrupt the config.
        let tmp = tempfile::tempdir().unwrap();
        let mut frontend = FakeInitFrontend {
            replace_aspec: false,
            run_audit: false,
            work_items_config: None,
            phases: Vec::new(),
        };
        // First run.
        let mut engine = make_engine(tmp.path());
        engine.run_to_completion(&mut frontend).await.unwrap();
        // Second run.
        let mut engine2 = make_engine(tmp.path());
        let summary2 = engine2.run_to_completion(&mut frontend).await.unwrap();
        assert_eq!(engine2.phase(), &InitPhase::Complete);
        assert!(matches!(summary2.config, StepStatus::Done));
    }

    #[tokio::test]
    async fn writing_work_items_config_persists_when_some() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = make_engine(tmp.path());
        let wi_cfg = crate::data::config::repo::WorkItemsConfig {
            dir: Some("my-work-items".to_string()),
            template: None,
        };
        let mut frontend = FakeInitFrontend {
            replace_aspec: true,
            run_audit: false,
            work_items_config: Some(wi_cfg),
            phases: Vec::new(),
        };
        let summary = engine.run_to_completion(&mut frontend).await.unwrap();
        assert!(matches!(summary.work_items_setup, StepStatus::Done));
        // Load the saved config and confirm work_items was persisted.
        let saved = crate::data::config::repo::RepoConfig::load(tmp.path()).unwrap_or_default();
        assert!(
            saved.work_items.is_some(),
            "work_items config must be persisted when user accepts"
        );
        assert_eq!(
            saved.work_items.as_ref().and_then(|w| w.dir.as_deref()),
            Some("my-work-items")
        );
    }
}
