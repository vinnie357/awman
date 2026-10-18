//! B1: original SDK owner survives actual post-start local resource teardown.
use super::native_codex_auth_entry_test::command;
use super::{Command, ExecPromptCommandFrontend};
use crate::command::error::CommandError;
use crate::data::message::{UserMessage, UserMessageSink};
use crate::data::session::AgentName;
use crate::engine::agent::native_codex_auth_preparation_test::TestResult;
use crate::engine::agent::native_codex_main_a2a_test_support::{
    is_main, A2Fixture, ObservedBoundary,
};
use crate::engine::agent::AgentEngine;
use crate::engine::agent_runtime::AgentFrontend;
use crate::engine::container::gated_launch::*;
use crate::engine::container::orchestrated_apple::*;
use crate::engine::container::ContainerRuntime;
use crate::engine::error::EngineError;
use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
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
impl ExecPromptCommandFrontend for TwoStreams {}

struct ExpectedB1Retention {
    _fixture: A2Fixture,
    _registry: Arc<LaunchRetentionRegistry>,
    _probe: retention_test_support::RetentionProbe,
    _ticket: LaunchRetentionTicket,
    _claims: retention_test_support::SupervisorActiveClaimBarrier,
}
static EXPECTED_B1_RETENTIONS: OnceLock<Mutex<Vec<ExpectedB1Retention>>> = OnceLock::new();

struct PostStartFault {
    inner: Arc<ObservedBoundary>,
    faulted: AtomicBool,
    inspected: AtomicBool,
}
fn install(f: &mut A2Fixture) -> Arc<PostStartFault> {
    let fault = Arc::new(PostStartFault {
        inner: Arc::clone(&f.boundary),
        faulted: AtomicBool::new(false),
        inspected: AtomicBool::new(false),
    });
    f.fixture.auth.runtime = Arc::new(ContainerRuntime::apple_with_test_boundary(fault.clone()));
    let engine = AgentEngine::new(
        Arc::clone(&f.fixture.auth.overlay),
        Arc::clone(&f.fixture.auth.runtime),
    )
    .with_native_codex_auth_test_observer(Arc::clone(&f.fixture.auth.observer))
    .with_native_codex_main_test_opt_in();
    f.fixture.auth.engine = engine;
    fault
}
impl OrchestratedAppleTestBoundary for PostStartFault {
    fn take_launch_resource(
        &self,
        name: &str,
        argv: &[String],
    ) -> Option<Box<dyn std::any::Any + Send>> {
        self.inner.take_launch_resource(name, argv)
    }
    fn observe_spawn(
        &self,
        argv: Vec<String>,
        name: String,
        labels: Vec<String>,
        mounts: Vec<(String, bool)>,
        registry: bool,
        request: Vec<u8>,
        manifests: BTreeMap<String, Vec<u8>>,
    ) {
        self.inner
            .observe_spawn(argv, name, labels, mounts, registry, request, manifests);
    }
    fn observe_child_start(&self) {
        self.inner.observe_child_start();
        let snapshots = self.inner.snapshots.lock().expect("actual spawn snapshots");
        let actual = snapshots.last().expect("actual child started");
        if is_main(&actual.argv) {
            let plan = actual
                .controls
                .parent()
                .expect("protected parent")
                .join("launch-plan.json");
            assert!(plan.is_file(), "actual protected plan must already exist");
            fs::write(plan, b"B1 post-start invalid plan").expect("test-owned plan fault");
            assert!(
                !self.faulted.swap(true, Ordering::AcqRel),
                "only one actual main fault"
            );
        }
    }
    fn observe_protected_launch_identity(&self, inspection: &ProviderLaunchInspection) {
        if self.faulted.load(Ordering::Acquire) {
            self.inspected.store(true, Ordering::Release);
        }
        self.inner.observe_protected_launch_identity(inspection);
    }
    fn retention_available(&self) -> bool {
        self.inner.retention_available()
    }
    fn image_inspection(&self) -> Result<Vec<u8>, EngineError> {
        self.inner.image_inspection()
    }
    fn provider(&self) -> Arc<dyn GatedProviderAdapter> {
        self.inner.provider()
    }
    fn executable(&self) -> &'static str {
        self.inner.executable()
    }
    fn prepare_simulated_guest(
        &self,
        plan: &DurableLaunchPlan,
    ) -> Result<(), LaunchRetentionReason> {
        self.inner.prepare_simulated_guest(plan)
    }
    fn pause_before_ready(&self) -> bool {
        self.inner.pause_before_ready()
    }
    fn pause_after_ready(&self) -> bool {
        self.inner.pause_after_ready()
    }
    fn observe_ready_rejected(&self) {
        self.inner.observe_ready_rejected()
    }
    fn observe_protected_ready(&self) {
        self.inner.observe_protected_ready()
    }
    fn observe_accepted_release(&self) {
        self.inner.observe_accepted_release()
    }
    fn observe_provider_entry(&self, before: bool) {
        self.inner.observe_provider_entry(before)
    }
    fn observe_cancel_point(&self, point: TestCancellationPoint) {
        self.inner.observe_cancel_point(point)
    }
    fn begin_cleanup(&self) {
        self.inner.begin_cleanup()
    }
    fn observe_reap(&self) {
        self.inner.observe_reap()
    }
    fn observe_actual_child_reaped(&self) {
        self.inner.observe_actual_child_reaped()
    }
    fn observe_never_created_proven(&self) {
        self.inner.observe_never_created_proven()
    }
    fn observe_exact_absence_proven(&self) {
        self.inner.observe_exact_absence_proven()
    }
    fn observe_cleanup_publication(&self, succeeded: bool) {
        self.inner.observe_cleanup_publication(succeeded)
    }
    fn observe_resource_loan_released(&self) {
        self.inner.observe_resource_loan_released()
    }
    fn cleanup_reap_proven(&self) -> bool {
        self.inner.cleanup_reap_proven()
    }
    fn observe_custody(&self, ownership: TestOwnershipObservation, legacy: bool) {
        self.inner.observe_custody(ownership, legacy)
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_start_local_reap_keeps_original_sdk_writer() -> TestResult {
    let (mut fixture, release, entered) = A2Fixture::new(true)?;
    let fault = install(&mut fixture);
    let registry = fixture.fixture.auth.runtime.launch_retention()?;
    let probe = retention_test_support::registry_probe(&registry);
    let _claims = probe.hold_supervisor_active_claims();
    assert!(probe.only_ticket().is_none());
    let (auth, _auth_output) = fixture.fixture.auth.frontend();
    let (main, _main_output) = fixture.fixture.auth.frontend();
    let frontend = TwoStreams {
        streams: Mutex::new(VecDeque::from([auth, main])),
    };
    let result = tokio::time::timeout(
        Duration::from_secs(15),
        command(&fixture.fixture.auth).run_with_frontend(Box::new(frontend)),
    )
    .await?;
    assert!(
        matches!(result, Err(CommandError::Engine(_))),
        "actual post-start error required: {result:?}"
    );
    tokio::time::timeout(Duration::from_secs(15), entered).await??;
    assert!(fault.faulted.load(Ordering::Acquire));
    assert!(!fault.inspected.load(Ordering::Acquire));
    let ticket = probe
        .only_ticket()
        .ok_or("one actual main retention ticket")?;
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            probe.advance_one_supervisor_active_attempt()?;
            let state = probe.snapshot();
            if state.managed_exited > 0 && state.unreaped == 0 {
                break Ok::<(), EngineError>(());
            }
            tokio::task::yield_now().await;
        }
    })
    .await??;
    let snapshots = fixture
        .boundary
        .snapshots
        .lock()
        .expect("actual launch evidence");
    assert_eq!(snapshots.len(), 2);
    let (auth, main) = (&snapshots[0], &snapshots[1]);
    assert!(!is_main(&auth.argv) && is_main(&main.argv));
    assert!(auth.reaped && auth.cleaned && auth.released);
    assert!(!main.cleaned && !main.released);
    assert_eq!(auth.active, main.active);
    assert_eq!(auth.owner, main.owner);
    assert_eq!(fixture.fixture.auth.observer.activation(), auth.active);
    assert!(probe.contains(ticket));
    fixture.fixture.auth.observer.assert_locked();
    fixture.fixture.auth.observer.assert_unpublished();
    assert_eq!(probe.only_ticket(), Some(ticket));
    assert_eq!(probe.snapshot().retained, 1);
    drop(snapshots);
    release.release();
    assert_eq!(probe.only_ticket(), Some(ticket));
    assert_eq!(probe.snapshot().retained, 1);
    assert_eq!(probe.snapshot().unreaped, 0);
    fixture.fixture.auth.observer.assert_locked();
    fixture.fixture.auth.observer.assert_unpublished();
    let mut expected = EXPECTED_B1_RETENTIONS
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    expected.push(ExpectedB1Retention {
        _fixture: fixture,
        _registry: registry,
        _probe: probe,
        _ticket: ticket,
        _claims,
    });
    Ok(())
}
