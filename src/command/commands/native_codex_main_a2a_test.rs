//! A2r: actual main retirement gates command and strict storage completion.
use super::native_codex_auth_entry_test::command;
use super::{Command, ExecPromptCommandFrontend};
use crate::command::error::CommandError;
use crate::data::message::{UserMessage, UserMessageSink};
use crate::data::session::AgentName;
use crate::engine::agent::native_codex_auth_preparation_test::{TestResult, QUESTION};
use crate::engine::agent::native_codex_main_a2a_test_support::*;
use crate::engine::agent_runtime::AgentFrontend;
use std::collections::VecDeque;
use std::future::Future;
use std::path::Path;
use std::sync::Mutex;
use std::task::Poll;
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
impl ExecPromptCommandFrontend for TwoStreams {}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_main_destructor_gates_command_and_strict_completion() -> TestResult {
    let (mut fixture, release, mut entered) = A2Fixture::new(true)?;
    let (auth, _auth_output) = fixture.fixture.auth.frontend();
    let (main, mut output) = fixture.fixture.auth.frontend();
    let frontend = TwoStreams {
        streams: Mutex::new(VecDeque::from([auth, main])),
    };
    let command = command(&fixture.fixture.auth);
    let future = command.run_with_frontend(Box::new(frontend));
    tokio::pin!(future);
    tokio::select! {
        result = &mut future => panic!("actual command terminated before main resource destructor; children={}, result={result:?}", fixture.fixture.boundary.spawn_count()),
        entry = &mut entered => { entry?; }
        _ = tokio::time::sleep(Duration::from_secs(15)) => panic!("neither command termination nor actual destructor entry observed"),
    }
    std::future::poll_fn(|cx| {
        assert!(
            matches!(future.as_mut().poll(cx), Poll::Pending),
            "command completed while actual resource destructor held"
        );
        Poll::Ready(())
    })
    .await;
    let bytes = output.bytes();
    assert!(
        String::from_utf8_lossy(&bytes).contains("turn.completed"),
        "actual required stdout was not drained"
    );
    assert_held_retirement(&fixture)?;
    release.release();
    let result = tokio::time::timeout(Duration::from_secs(15), &mut future).await?;
    assert!(
        result.is_ok(),
        "actual main command failed after resource release: {result:?}"
    );
    assert_completed(&fixture)?;
    fixture.settled();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_only_fixture_still_closes_main() -> TestResult {
    let (mut fixture, _release, _entered) = A2Fixture::new(false)?;
    let (auth, _output) = fixture.fixture.auth.frontend();
    let frontend = TwoStreams {
        streams: Mutex::new(VecDeque::from([auth])),
    };
    let result = command(&fixture.fixture.auth)
        .run_with_frontend(Box::new(frontend))
        .await;
    assert!(
        matches!(result, Err(CommandError::Engine(crate::engine::error::EngineError::Config(ref reason))) if reason == "native Codex main execution remains unqualified")
    );
    let records = fixture.fixture.boundary.child_records()?;
    assert_eq!(records.len(), 1);
    assert_eq!(fixture.fixture.boundary.spawn_count(), 1);
    assert!(fixture.fixture.boundary.first_retired());
    assert_eq!(records[0]["phase"], "auth");
    assert_eq!(records[0]["stdin"], "");
    assert!(!records[0]["argv"]
        .as_array()
        .expect("auth argv")
        .iter()
        .any(|arg| arg.as_str().is_some_and(|s| s.contains(QUESTION))));
    assert!(!fixture
        .boundary
        .snapshots
        .lock()
        .expect("snapshots")
        .iter()
        .any(|s| is_main(&s.argv)));
    fixture.settled();
    Ok(())
}
