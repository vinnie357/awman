//! A2m scope packet: exact mounts and actual protected launch identity.
use super::native_codex_auth_entry_test::command;
use super::{Command, ExecPromptCommandFrontend};
use crate::command::error::CommandError;
use crate::data::message::{UserMessage, UserMessageSink};
use crate::data::session::AgentName;
use crate::engine::agent::native_codex_auth_preparation_test::TestResult;
use crate::engine::agent::native_codex_main_a2a_test_support::{assert_held_retirement, A2Fixture};
use crate::engine::agent::native_codex_main_a2m_test_support::*;
use crate::engine::agent_runtime::AgentFrontend;
use std::collections::VecDeque;
use std::path::Path;
use std::sync::Mutex;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_main_has_exact_mounts_and_fresh_launch_identity() -> TestResult {
    let (mut fixture, release, mut entered) = A2Fixture::new(true)?;
    let observed = instrument(&mut fixture)?;
    let (auth, _auth_output) = fixture.fixture.auth.frontend();
    let (main, _main_output) = fixture.fixture.auth.frontend();
    let frontend = TwoStreams {
        streams: Mutex::new(VecDeque::from([auth, main])),
    };
    let command = command(&fixture.fixture.auth);
    let future = command.run_with_frontend(Box::new(frontend));
    tokio::pin!(future);
    tokio::select! {
        result = &mut future => panic!("command terminated before real main resource hold; children={}, result={result:?}", fixture.fixture.boundary.spawn_count()),
        entry = &mut entered => { entry?; }
        _ = tokio::time::sleep(Duration::from_secs(15)) => panic!("neither actual hold nor command termination observed"),
    }
    assert_held_retirement(&fixture)?;
    assert_materials(&fixture, &observed)?;
    release.release();
    let result = tokio::time::timeout(Duration::from_secs(15), &mut future).await?;
    assert!(result.is_ok(), "actual main command failed: {result:?}");
    fixture.settled();
    Ok(())
}
