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

#[derive(Default)]
struct Drain {
    calls: std::sync::atomic::AtomicUsize,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    fail: bool,
}
struct PrivateFrontend {
    observer: Arc<AuthObserver>,
    user: Mutex<Option<Box<dyn AgentFrontend>>>,
    auth: Mutex<Option<Box<dyn AgentFrontend>>>,
    drain: Arc<Drain>,
}
impl UserMessageSink for PrivateFrontend {
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
impl crate::command::commands::agent_setup::HasAgentFrontend for PrivateFrontend {
    fn container_frontend(&mut self) -> Box<dyn AgentFrontend> {
        self.user
            .lock()
            .expect("user")
            .take()
            .expect("single user stream")
    }
}
impl crate::command::commands::agent_setup::AgentSetupFrontend for PrivateFrontend {
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
impl crate::command::commands::agent_auth::AgentAuthFrontend for PrivateFrontend {
    fn ask_agent_auth_consent(
        &mut self,
        _: &AgentName,
        _: &[&str],
    ) -> Result<crate::command::commands::agent_auth::AgentAuthDecision, CommandError> {
        panic!("generic host credential consent reached")
    }
}
impl crate::command::commands::mount_scope::MountScopeFrontend for PrivateFrontend {
    fn ask_mount_scope(
        &mut self,
        _: &Path,
        _: &Path,
    ) -> Result<crate::command::commands::mount_scope::MountScopeDecision, CommandError> {
        panic!("auth requested review-source mount")
    }
}
impl crate::engine::acp::AcpFrontend for PrivateFrontend {
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
impl ExecPromptCommandFrontend for PrivateFrontend {
    fn native_auth_frontend(&mut self) -> Result<Box<dyn AgentFrontend>, CommandError> {
        self.observer.event("private-channel-acquired");
        Ok(self
            .auth
            .lock()
            .expect("auth channel")
            .take()
            .expect("one auth channel"))
    }

    async fn finish_native_output(&mut self) -> Result<(), CommandError> {
        if self
            .drain
            .calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            == 0
        {
            self.observer.assert_locked();
            self.drain.entered.notify_one();
            self.drain.release.notified().await;
            self.observer.assert_locked();
            if self.drain.fail {
                return Err(CommandError::Other("synthetic auth sink failure".into()));
            }
        }
        Ok(())
    }
}

fn private_frontend(
    fixture: &Fixture,
    drain: Arc<Drain>,
) -> (
    PrivateFrontend,
    crate::engine::agent::native_codex_auth_preparation_test::UserOutput,
    crate::engine::agent::native_codex_auth_preparation_test::UserOutput,
) {
    let (auth, private_output) = fixture.frontend();
    let (ordinary, ordinary_output) = fixture.frontend();
    (
        PrivateFrontend {
            observer: Arc::clone(&fixture.observer),
            user: Mutex::new(Some(ordinary)),
            auth: Mutex::new(Some(auth)),
            drain,
        },
        private_output,
        ordinary_output,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_terminal_routes_both_challenges_only_to_private_channel() -> TestResult {
    let fixture = Fixture::new(&[])?;
    fixture.release_child();
    let drain = Arc::new(Drain::default());
    drain.release.notify_one();
    let (frontend, mut private, mut ordinary) = private_frontend(&fixture, Arc::clone(&drain));
    let result = command(&fixture)
        .run_with_frontend(Box::new(frontend))
        .await;
    assert!(result.is_err(), "unqualified main gate must remain closed");
    let private_bytes = String::from_utf8(private.bytes())?;
    for marker in [
        crate::engine::agent::native_codex_auth_preparation_test::URL,
        "SYNTHETIC_STDERR_CHALLENGE",
    ] {
        assert!(
            private_bytes.contains(marker),
            "private auth sink lost {marker}"
        );
    }
    assert!(
        ordinary.bytes().is_empty(),
        "challenge reached ordinary stream"
    );
    fixture.observer.assert_auth_output_private();
    fixture.observer.assert_unpublished();
    let facts = fixture.observer.facts.lock().expect("facts");
    assert_eq!(facts.argv.len(), 1);
    assert!(!facts.events.contains(&"generic-auth"));
    assert_eq!(drain.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_terminal_writer_drain_holds_same_owner_before_main_then_preserves_main_bytes(
) -> TestResult {
    let fixture = crate::engine::agent::native_codex_main_test_support::MainFixture::new()?;
    let drain = Arc::new(Drain::default());
    let (frontend, mut private, mut ordinary) = private_frontend(&fixture.auth, Arc::clone(&drain));
    let future = command(&fixture.auth).run_with_frontend(Box::new(frontend));
    tokio::pin!(future);
    tokio::select! {
        _ = drain.entered.notified() => {},
        result = &mut future => panic!("auth writers were not drained before main: {result:?}"),
        _ = tokio::time::sleep(std::time::Duration::from_secs(15)) => panic!("auth drain not reached"),
    }
    assert!(
        fixture.boundary.first_retired(),
        "writer drain preceded actual auth retirement"
    );
    assert_eq!(
        fixture.boundary.spawn_count(),
        1,
        "main started before auth writer drain"
    );
    fixture.auth.observer.assert_locked();
    fixture.auth.observer.assert_unpublished();
    let state = fixture.auth.observer.state_path();
    let activation = fixture.auth.observer.activation();
    drain.release.notify_one();
    let result = tokio::time::timeout(std::time::Duration::from_secs(15), &mut future).await?;
    assert!(result.is_ok(), "main failed after writer drain: {result:?}");
    assert_eq!(fixture.boundary.spawn_count(), 2);
    assert!(fixture.boundary.main_followed_retired_auth());
    assert_eq!(fixture.auth.observer.state_path(), state);
    let facts = fixture.auth.observer.facts.lock().expect("facts");
    assert!(
        facts
            .material
            .iter()
            .all(|(path, active)| path == &state && active == &activation),
        "handoff changed owner state or activation"
    );
    drop(facts);
    assert!(String::from_utf8(private.bytes())?
        .contains(crate::engine::agent::native_codex_auth_preparation_test::URL));
    assert_eq!(ordinary.bytes(), b"{\"type\":\"thread.started\",\"thread_id\":\"connected-main-fixture\"}\n{\"type\":\"turn.started\"}\n{\"type\":\"turn.completed\"}\n");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_terminal_writer_failure_denies_handoff_without_fallback_or_early_owner_release(
) -> TestResult {
    let fixture = crate::engine::agent::native_codex_main_test_support::MainFixture::new()?;
    let drain = Arc::new(Drain {
        fail: true,
        ..Default::default()
    });
    let (frontend, _private, mut ordinary) = private_frontend(&fixture.auth, Arc::clone(&drain));
    let future = command(&fixture.auth).run_with_frontend(Box::new(frontend));
    tokio::pin!(future);
    tokio::select! {
        _ = drain.entered.notified() => {},
        result = &mut future => panic!("main bypassed auth sink failure: {result:?}"),
        _ = tokio::time::sleep(std::time::Duration::from_secs(15)) => panic!("auth drain not reached"),
    }
    assert!(fixture.boundary.first_retired());
    fixture.auth.observer.assert_locked();
    fixture.auth.observer.assert_unpublished();
    assert_eq!(fixture.boundary.spawn_count(), 1);
    drain.release.notify_one();
    let result = tokio::time::timeout(std::time::Duration::from_secs(15), &mut future).await?;
    let error = result
        .expect_err("auth sink failure authorized main")
        .to_string();
    assert_eq!(error, "native authentication output failed; restore a writable controlling terminal before continuing");
    assert!(!error.contains("synthetic auth sink failure"));
    assert_eq!(
        fixture.boundary.spawn_count(),
        1,
        "sink failure launched main"
    );
    assert!(
        ordinary.bytes().is_empty(),
        "sink failure fell back to ordinary output"
    );
    fixture.auth.observer.assert_unpublished();
    assert!(!fixture
        .auth
        .observer
        .facts
        .lock()
        .expect("facts")
        .events
        .contains(&"generic-auth"));
    Ok(())
}
