//! Private native authentication must be acquired before owner state is touched.
use super::native_codex_auth_entry_test::command;
use super::Command;
use super::ExecPromptCommandFrontend;
use crate::command::error::CommandError;
use crate::data::message::{UserMessage, UserMessageSink};
use crate::data::session::AgentName;
use crate::engine::agent::native_codex_auth_preparation_test::{AuthObserver, Fixture, TestResult};
use crate::engine::agent_runtime::AgentFrontend;
use std::path::Path;
use std::sync::{Arc, Mutex};

struct AbsentFrontend {
    observer: Arc<AuthObserver>,
    user: Mutex<Option<Box<dyn AgentFrontend>>>,
    generic_channels: Arc<std::sync::atomic::AtomicUsize>,
}
impl UserMessageSink for AbsentFrontend {
    fn write_message(&mut self, message: UserMessage) {
        self.observer
            .facts
            .lock()
            .expect("facts")
            .diagnostics
            .push(message.text);
    }
    fn replay_queued(&mut self) {}
}
impl crate::command::commands::agent_setup::HasAgentFrontend for AbsentFrontend {
    fn container_frontend(&mut self) -> Box<dyn AgentFrontend> {
        self.generic_channels
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.user
            .lock()
            .expect("user")
            .take()
            .expect("single user stream")
    }
}
impl crate::command::commands::agent_setup::AgentSetupFrontend for AbsentFrontend {
    fn ask_agent_setup(
        &mut self,
        _: &AgentName,
        _: &AgentName,
        _: bool,
        _: bool,
    ) -> Result<crate::command::commands::agent_setup::AgentSetupDecision, CommandError> {
        panic!("generic agent setup reached")
    }
    fn record_fallback(&mut self, _: &AgentName, _: &AgentName) {
        panic!("generic fallback reached")
    }
}
impl crate::command::commands::agent_auth::AgentAuthFrontend for AbsentFrontend {
    fn ask_agent_auth_consent(
        &mut self,
        _: &AgentName,
        _: &[&str],
    ) -> Result<crate::command::commands::agent_auth::AgentAuthDecision, CommandError> {
        panic!("generic host credential consent reached")
    }
}
impl crate::command::commands::mount_scope::MountScopeFrontend for AbsentFrontend {
    fn ask_mount_scope(
        &mut self,
        _: &Path,
        _: &Path,
    ) -> Result<crate::command::commands::mount_scope::MountScopeDecision, CommandError> {
        panic!("auth requested review-source mount")
    }
}
impl crate::engine::acp::AcpFrontend for AbsentFrontend {
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
        panic!("auth requested model question")
    }
}
#[async_trait::async_trait]
impl ExecPromptCommandFrontend for AbsentFrontend {}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn absent_native_auth_channel_rejects_before_activation_material_or_child() -> TestResult {
    let fixture = Fixture::new(&[])?;
    fixture.release_child();
    let (user, mut ordinary) = fixture.frontend();
    let generic_channels = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let frontend = AbsentFrontend {
        generic_channels: Arc::clone(&generic_channels),
        observer: Arc::clone(&fixture.observer),
        user: Mutex::new(Some(user)),
    };
    let result = command(&fixture)
        .run_with_frontend(Box::new(frontend))
        .await;
    let error = result
        .expect_err("missing private interactive channel must reject")
        .to_string();
    assert!(
        error.contains("controlling terminal"),
        "actionable channel error required: {error}"
    );
    assert!(
        error.contains("writable"),
        "error must identify required writable channel: {error}"
    );
    let paths = crate::data::fs::data_paths::DataPaths::from_env(fixture.session.env())?;
    assert!(
        !paths.root().join("native-sdk-state").exists(),
        "channel denial activated strict owner storage"
    );
    let facts = fixture.observer.facts.lock().expect("facts");
    assert!(
        facts.material.is_empty(),
        "channel denial accessed owner material"
    );
    assert!(
        facts.argv.is_empty(),
        "channel denial launched authentication"
    );
    assert!(!facts.events.contains(&"generic-auth"));
    assert_eq!(
        generic_channels.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "missing private channel requested generic output"
    );
    assert!(
        ordinary.bytes().is_empty(),
        "private auth fell back to ordinary stream"
    );
    Ok(())
}
