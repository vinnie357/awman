//! First connected-main RED: the accepted command authenticates but never launches main.
use super::native_codex_auth_entry_test::command;
use super::{Command, ExecPromptCommandFrontend};
use crate::command::error::CommandError;
use crate::data::message::{UserMessage, UserMessageSink};
use crate::data::session::AgentName;
use crate::engine::agent::native_codex_auth_preparation_test::{TestResult, QUESTION};
use crate::engine::agent::native_codex_main_test_support::MainFixture;
use crate::engine::agent_runtime::AgentFrontend;
use std::collections::VecDeque;
use std::path::Path;
use std::sync::Mutex;

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
async fn actual_command_auth_retirement_connects_to_distinct_main_child() -> TestResult {
    let fixture = MainFixture::new()?;
    let (auth_stream, _auth_output) = fixture.auth.frontend();
    let (main_stream, _main_output) = fixture.auth.frontend();
    let frontend = TwoStreams {
        streams: Mutex::new(VecDeque::from([auth_stream, main_stream])),
    };
    let result = command(&fixture.auth)
        .run_with_frontend(Box::new(frontend))
        .await;
    let records = fixture.boundary.child_records()?;
    assert!(!fixture
        .auth
        .observer
        .facts
        .lock()
        .expect("facts")
        .events
        .contains(&"generic-auth"));
    assert!(!records.is_empty(), "auth child did not execute");
    assert_eq!(records[0]["phase"], "auth");
    assert_eq!(records[0]["stdin"], "", "question leaked to auth child");
    let auth_argv = records[0]["argv"].as_array().expect("actual auth argv");
    assert!(auth_argv
        .windows(3)
        .any(|args| args == ["codex", "login", "--device-auth"]));
    assert!(
        !auth_argv
            .iter()
            .any(|arg| arg.as_str().is_some_and(|arg| arg.contains(QUESTION))),
        "question leaked into auth argv"
    );
    assert!(
        fixture.boundary.first_retired(),
        "actual auth cleanup/reap/resource release incomplete"
    );
    if records.len() == 1 {
        assert!(
            matches!(&result,
            Err(CommandError::Engine(crate::engine::error::EngineError::Config(reason)))
            if reason == "native Codex main execution remains unqualified"),
            "unexpected failure before missing main route: {result:?}"
        );
    }
    assert_eq!(
        records.len(),
        2,
        "retired auth must connect to actual main child; command result: {result:?}"
    );
    assert_eq!(
        fixture.boundary.spawn_count(),
        2,
        "extra actual child launch attempts"
    );
    assert_eq!(records[1]["phase"], "main");
    assert_ne!(records[0]["pid"], records[1]["pid"]);
    assert_ne!(records[0]["controls"], records[1]["controls"]);
    let main_argv = records[1]["argv"].as_array().expect("actual main argv");
    assert!(main_argv.windows(2).any(|args| args == ["codex", "exec"]));
    assert!(
        !main_argv
            .iter()
            .any(|arg| arg.as_str().is_some_and(|arg| arg.contains(QUESTION))),
        "question duplicated into main argv"
    );
    assert_eq!(
        records[1]["stdin"], QUESTION,
        "main input must deliver exactly one question"
    );
    assert!(
        fixture.boundary.main_followed_retired_auth(),
        "main spawned before actual auth retirement boundary"
    );
    Ok(())
}
