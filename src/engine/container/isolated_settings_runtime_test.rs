//! Actual AgentEngine options propagation with an inert image-home backend.
use super::*;
use crate::data::session::{
    AgentHandle, AgentName, Session, SessionOpenOptions, StaticGitRootResolver,
};
use crate::engine::agent::{AgentEngine, AgentRunOptions};
use crate::engine::agent_runtime::execution::{AgentInstance, AgentStats};
use crate::engine::container::backend::ContainerBackend;
use crate::engine::container::options::{
    ContainerOption, OverlayPermission, ResolvedContainerOptions,
};
use crate::engine::overlay::OverlayEngine;

struct IsolatedSettingsBackend;
impl ContainerBackend for IsolatedSettingsBackend {
    fn build(&self, _: ResolvedContainerOptions) -> Result<Box<dyn AgentInstance>, EngineError> {
        panic!("settings options test must not create a provider instance")
    }
    fn list_running(&self, _: &Session) -> Result<Vec<AgentHandle>, EngineError> {
        panic!("settings options test must not inspect providers")
    }
    fn stats(&self, _: &AgentHandle) -> Result<AgentStats, EngineError> {
        panic!("settings options test must not inspect providers")
    }
    fn attach(&self, _: &AgentHandle) -> Result<Box<dyn AgentInstance>, EngineError> {
        panic!("settings options test must not attach providers")
    }
    fn list_running_with_name_prefix(&self, _: &str) -> Result<Vec<AgentHandle>, EngineError> {
        panic!("settings options test must not inspect providers")
    }
    fn name(&self) -> &'static str {
        "isolated-settings-fixture"
    }
    fn image_home_dir(&self, _: &str) -> Option<String> {
        Some("/isolated-agent".into())
    }
}

#[test]
fn isolated_settings_captured_run_options_reach_actual_agent_engine_overlay_effect() {
    for isolated in [false, true] {
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let source = home.path().join(".codex");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("marker"), b"host marker").unwrap();
        let resolver = StaticGitRootResolver::new(root.path());
        let env = crate::data::config::env::EnvSnapshot::with_overrides([(
            crate::data::config::env::AWMAN_CONFIG_HOME,
            home.path().to_str().unwrap(),
        )]);
        let session = Session::open(
            root.path().to_path_buf(),
            &resolver,
            SessionOpenOptions {
                env: Some(env),
                ..Default::default()
            },
        )
        .unwrap();
        let overlay = OverlayEngine::with_auth_resolver(
            crate::data::fs::auth_paths::AuthPathResolver::at_home(home.path()),
        )
        .with_secret_files_provider(Arc::new(|_| Vec::new()))
        .with_credential_provider(Arc::new(|_| None));
        let mut runtime = ContainerRuntime::docker();
        runtime.backend = Arc::new(IsolatedSettingsBackend);
        let engine = AgentEngine::new(Arc::new(overlay), Arc::new(runtime));
        let run = AgentRunOptions {
            isolated_settings: isolated,
            ..Default::default()
        };
        let options = engine
            .build_options(&session, &AgentName::new("codex").unwrap(), &run)
            .unwrap();
        let settings = options
            .iter()
            .find_map(|option| match option {
                ContainerOption::Overlay(overlay)
                    if overlay.container_path == std::path::Path::new("/isolated-agent/.codex") =>
                {
                    Some(overlay)
                }
                _ => None,
            })
            .unwrap();
        assert_eq!(settings.permission, OverlayPermission::ReadWrite);
        assert_eq!(settings.host_path == source, !isolated);
        if isolated {
            std::fs::write(settings.host_path.join("marker"), b"container mutation").unwrap();
        }
        assert_eq!(
            std::fs::read(source.join("marker")).unwrap(),
            b"host marker"
        );
    }
}
