//! Positive control for the exact managed fixture-main entry observation route.
use super::native_codex_auth_entry_test::command;
use super::{Command, ExecPromptCommandFrontend};
use crate::command::error::CommandError;
use crate::data::message::{UserMessage, UserMessageSink};
use crate::data::session::AgentName;
use crate::engine::agent::native_codex_auth_preparation_test::{Fixture, TestResult, UserOutput};
use crate::engine::agent::native_codex_main_a2a_test_support::{
    assert_completed, assert_held_retirement, A2Fixture,
};
use crate::engine::agent::native_codex_main_a2m_test_support::{assert_materials, instrument};
use crate::engine::agent::native_codex_main_entry_marker_support::install;
use crate::engine::agent_runtime::AgentFrontend;
use serde_json::Value;
use std::collections::VecDeque;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
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

pub(super) fn two_streams(f: &Fixture) -> (Box<dyn ExecPromptCommandFrontend>, [UserOutput; 2]) {
    let (auth, auth_output) = f.frontend();
    let (main, main_output) = f.frontend();
    let frontend = TwoStreams {
        streams: Mutex::new(VecDeque::from([auth, main])),
    };
    (Box::new(frontend), [auth_output, main_output])
}

pub(super) async fn prove_entry_control() -> TestResult {
    let (mut fixture, release, mut entered) = A2Fixture::new(true)?;
    let scope = instrument(&mut fixture)?;
    let calls = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&calls);
    let snapshots = Arc::clone(&fixture.boundary);
    let entry = install(
        &mut fixture,
        Arc::clone(&scope),
        Box::new(move |argv| {
            assert_eq!(count.fetch_add(1, Ordering::AcqRel), 0);
            let records = snapshots.snapshots.lock().expect("bound main observation");
            assert_eq!(records.len(), 2);
            assert_eq!(records[1].argv, argv);
            assert!(!records[1].controls.join("release.json").exists());
        }),
    )?;
    let (frontend, _outputs) = two_streams(&fixture.fixture.auth);
    let command = command(&fixture.fixture.auth);
    let future = command.run_with_frontend(frontend);
    tokio::pin!(future);
    tokio::select! {
        result = &mut future => panic!("control ended before resource hold: {result:?}"),
        entry = &mut entered => { entry?; }
        _ = tokio::time::sleep(Duration::from_secs(15)) => panic!("control hold absent"),
    }
    assert_eq!(calls.load(Ordering::Acquire), 1);
    assert!(entry.main_started.load(Ordering::Acquire));
    assert!(entry.main_reaped.load(Ordering::Acquire));
    assert!(!entry.main_never_created.load(Ordering::Acquire));
    assert_held_retirement(&fixture)?;
    assert_materials(&fixture, &scope)?;
    let files = fs::read_dir(&entry.marker_dir)?.collect::<Result<Vec<_>, _>>()?;
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].file_name(), "main.json");
    let marker: Value = serde_json::from_slice(&fs::read(entry.marker_dir.join("main.json"))?)?;
    assert_eq!(marker.as_object().ok_or("entry marker object")?.len(), 3);
    assert_eq!(marker["phase"], "main");
    assert!(marker["pid"].as_u64().ok_or("entry PID")? > 0);
    let records = fixture.fixture.boundary.child_records()?;
    let main = records
        .iter()
        .find(|r| r["phase"] == "main")
        .ok_or("actual main record")?;
    assert_eq!(marker["pid"], main["pid"]);
    {
        let snapshots = fixture.boundary.snapshots.lock().expect("actual snapshots");
        assert_eq!(marker["container_name"], snapshots[1].name);
    }
    release.release();
    let result = tokio::time::timeout(Duration::from_secs(15), &mut future).await?;
    assert!(result.is_ok(), "entry positive control failed: {result:?}");
    assert_completed(&fixture)?;
    fixture.settled();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn managed_fixture_main_entry_marker_and_pre_cli_observer_are_live() -> TestResult {
    prove_entry_control().await
}
