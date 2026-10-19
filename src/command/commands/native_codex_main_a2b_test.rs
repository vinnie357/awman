//! A2b.1: equivalent ancestor aliases preserve the canonical strict owner.
use super::native_codex_auth_entry_test::command;
use super::{Command, ExecPromptCommandFrontend};
use crate::command::error::CommandError;
use crate::data::config::env::EnvSnapshot;
use crate::data::message::{UserMessage, UserMessageSink};
use crate::data::session::{AgentName, Session, SessionOpenOptions};
use crate::engine::agent::native_codex_auth_preparation_test::TestResult;
use crate::engine::agent::native_codex_main_a2a_test_support::{
    assert_completed, assert_held_retirement, A2Fixture,
};
use crate::engine::agent::native_codex_main_a2b_test_support::install;
use crate::engine::agent::native_codex_main_a2m_test_support::{assert_materials, instrument};
use crate::engine::agent_runtime::AgentFrontend;
use std::collections::VecDeque;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct TwoStreams {
    streams: Mutex<VecDeque<Box<dyn AgentFrontend>>>,
}
impl UserMessageSink for TwoStreams {
    fn write_message(&mut self, _: UserMessage) {}
    fn replay_queued(&mut self) {}
}
impl crate::command::commands::agent_setup::HasAgentFrontend for TwoStreams {
    fn container_frontend(&mut self) -> Box<dyn AgentFrontend> {
        self.streams
            .lock()
            .expect("frontend streams")
            .pop_front()
            .expect("only auth and main may request streams")
    }
}
impl crate::command::commands::agent_setup::AgentSetupFrontend for TwoStreams {
    fn ask_agent_setup(
        &mut self,
        _: &AgentName,
        _: &AgentName,
        _: bool,
        _: bool,
    ) -> Result<crate::command::commands::agent_setup::AgentSetupDecision, CommandError> {
        panic!("generic setup reached")
    }
    fn record_fallback(&mut self, _: &AgentName, _: &AgentName) {
        panic!("fallback reached")
    }
}
impl crate::command::commands::agent_auth::AgentAuthFrontend for TwoStreams {
    fn ask_agent_auth_consent(
        &mut self,
        _: &AgentName,
        _: &[&str],
    ) -> Result<crate::command::commands::agent_auth::AgentAuthDecision, CommandError> {
        panic!("host credentials reached")
    }
}
impl crate::command::commands::mount_scope::MountScopeFrontend for TwoStreams {
    fn ask_mount_scope(
        &mut self,
        _: &Path,
        _: &Path,
    ) -> Result<crate::command::commands::mount_scope::MountScopeDecision, CommandError> {
        panic!("generic mount consent reached")
    }
}
impl crate::engine::acp::AcpFrontend for TwoStreams {
    fn render_update(&mut self, _: crate::engine::acp::SessionUpdate) {
        panic!("ACP reached")
    }
    fn request_permission(
        &mut self,
        _: crate::engine::acp::PermissionRequest,
    ) -> crate::engine::acp::PermissionDecision {
        panic!("ACP reached")
    }
    fn next_prompt(&mut self) -> Option<String> {
        panic!("unexpected second question")
    }
}
#[async_trait::async_trait]
impl ExecPromptCommandFrontend for TwoStreams {
    fn native_auth_frontend(&mut self) -> Result<Box<dyn AgentFrontend>, CommandError> {
        Ok(crate::command::commands::agent_setup::HasAgentFrontend::container_frontend(self))
    }
}

async fn alias_case(retarget: bool) -> TestResult {
    let (mut fixture, release, mut entered) = A2Fixture::new(true)?;
    let original = fixture.fixture.auth.session.clone();
    let physical = fs::canonicalize(fixture.fixture.auth.path())?;
    let alias = physical.join("logical-root");
    std::os::unix::fs::symlink(&physical, &alias)?;
    let replacement = physical.join("replacement-root");
    fs::create_dir(&replacement)?;
    fs::create_dir(replacement.join("project"))?;
    fs::create_dir(replacement.join("config"))?;
    fs::create_dir(replacement.join("config/data"))?;
    fs::write(replacement.join("project/replacement-only"), b"excluded")?;
    let logical_project = alias.join("project");
    let logical_home = alias.join("config");
    fixture.fixture.auth.session = Session::open_at_git_root(
        logical_project.clone(),
        logical_project.clone(),
        SessionOpenOptions {
            env: Some(EnvSnapshot::with_overrides(vec![(
                "AWMAN_CONFIG_HOME".to_owned(),
                logical_home.to_string_lossy().into_owned(),
            )])),
            available_agents: Some(vec![AgentName::new("codex")?]),
            ..Default::default()
        },
    )?;
    assert_eq!(fs::canonicalize(&logical_project)?, original.git_root());
    assert_eq!(
        fs::canonicalize(logical_home.join("data"))?,
        physical.join("config/data")
    );
    let scope = instrument(&mut fixture)?;
    install(
        &mut fixture,
        Arc::clone(&scope),
        alias.clone(),
        replacement.clone(),
        retarget,
    );
    let (auth, _auth_output) = fixture.fixture.auth.frontend();
    let (main, _main_output) = fixture.fixture.auth.frontend();
    let frontend = TwoStreams {
        streams: Mutex::new(VecDeque::from([auth, main])),
    };
    let command = command(&fixture.fixture.auth);
    let future = command.run_with_frontend(Box::new(frontend));
    tokio::pin!(future);
    tokio::select! {
        result = &mut future => panic!("main retired before resource hold: {result:?}"),
        entry = &mut entered => { entry?; }
        _ = tokio::time::sleep(Duration::from_secs(15)) => panic!("main resource hold absent"),
    }
    let expected_alias = if retarget { &replacement } else { &physical };
    assert_eq!(fs::read_link(&alias)?, *expected_alias);
    assert_held_retirement(&fixture)?;
    let active = fixture.fixture.auth.observer.activation();
    assert_eq!(
        active["owner"]["key"]["project"],
        physical.join("project").to_string_lossy().as_ref()
    );
    for (pin, path) in [
        (
            &active["owner"]["key"]["project_identity"],
            physical.join("project"),
        ),
        (&active["owner"]["data_root"], physical.join("config/data")),
    ] {
        let metadata = fs::symlink_metadata(path)?;
        assert!(!metadata.file_type().is_symlink());
        assert_eq!(pin["device"], metadata.dev());
        assert_eq!(pin["inode"], metadata.ino());
    }
    // Scope assertions use the original selection after the obsolete alias was retargeted.
    fixture.fixture.auth.session = original;
    assert_materials(&fixture, &scope)?;
    assert!(!physical.join("project/replacement-only").exists());
    assert!(fs::read_dir(replacement.join("config/data"))?
        .next()
        .is_none());
    release.release();
    let result = tokio::time::timeout(Duration::from_secs(15), &mut future).await?;
    assert!(result.is_ok(), "canonical owner main failed: {result:?}");
    assert_completed(&fixture)?;
    fixture.settled();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn equivalent_ancestor_aliases_keep_canonical_owner_and_mounts() -> TestResult {
    alias_case(false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unused_ancestor_alias_retarget_keeps_original_main_bindings() -> TestResult {
    alias_case(true).await
}
