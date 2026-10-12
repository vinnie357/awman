//! Outside-in acceptance contract for qualified orchestrated Apple command launches.
//!
//! This module owns the fixture, option mutations, public calls, assertions,
//! and observation collector. The missing
//! `ContainerRuntime::apple_with_orchestrated_command_test_boundary` seam may
//! replace only bounded Apple image/provider calls, the Apple CLI executable,
//! and deterministic pause notifications. Production code may publish facts
//! to `AppleCommandObserver` only at the corresponding real transition; it
//! cannot return a scenario-shaped report or manufacture gate, readiness,
//! lifecycle, retention, or cleanup authority.

use std::collections::BTreeMap;
use std::error::Error;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::data::config::repo::LaunchMode;
use crate::data::message::{UserMessage, UserMessageSink};
use crate::data::session::{AgentName, Session, SessionOpenOptions, StaticGitRootResolver};
use crate::data::startup_gate::{load_startup_gate, StartupGateSpec};
use crate::engine::agent_runtime::frontend::{AgentIo, AgentProgress, AgentStatus};
use crate::engine::agent_runtime::{AgentFrontend, AgentRuntimeEngine, ResolvedAgentOptions};
use crate::engine::auth::AgentCredentials;
use crate::engine::container::gated_launch::{
    retention_test_support, ActiveGatedRetryOutcome, LaunchRetentionTicket,
};
use crate::engine::container::options::{ContainerName, ContainerOption};
use crate::engine::container::ContainerRuntime;
use crate::engine::overlay::OverlayEngine;
use crate::engine::sandbox::SandboxRuntime;
use sha2::{Digest, Sha256};

use super::{AgentEngine, AgentRunOptions};

const NAME: &str = "awman-altana-command-apple";
const MANIFEST_NAME: &str = "workspace.manifest.json";
const RESERVED_LABEL: &str = "dev.awman.orchestrator-launch";
const RUNTIME_USER: &str = "1000:1000";
const CONTROL_PATH: &str = "/.awman/startup-gate/control";
const PROVIDER_ENTRY_MARKER: &str = "provider-entry.marker";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum QualificationFault {
    DockerRuntime,
    SandboxRuntime,
    Acp,
    AllowDocker,
    CallerName,
    DuplicateName,
    ReservedLabel,
    RemoveOnExit,
    MissingRetentionRegistry,
    UntrustedImage,
    ImageEntrypoint,
    ImageLoaderEnvironment,
    MissingRuntimeUser,
    InvalidRuntimeUser,
}

impl QualificationFault {
    fn image_inspections(self) -> usize {
        match self {
            Self::UntrustedImage
            | Self::ImageEntrypoint
            | Self::ImageLoaderEnvironment
            | Self::MissingRuntimeUser
            | Self::InvalidRuntimeUser => 1,
            _ => 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SealedMutation {
    Request,
    Manifest,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CancellationPoint {
    BeforeReady,
    AfterReadyBeforeRelease,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CleanupFault {
    TupleChanged,
    InspectionUnavailable,
    StopFailed,
    PreRemoveRuntimeIdChanged,
    PreRemoveCreatedAtChanged,
    ReapUncertain,
    PreRemoveInspectionUnavailable,
    RemoveFailed,
    FinalAbsenceUnproven,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RetentionRetryFault {
    None,
    InspectionUnavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SecondInspectionRelease {
    Matching,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RetryFaultPoint {
    CleanupInspection,
    FinalAbsence,
    RemoveEffectThenError,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RetryOrigin {
    Drop,
    NormalWait,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AppleCommandScenario {
    Qualification(QualificationFault),
    Qualified,
    ControlReplaced,
    SealedSourceMutated(SealedMutation),
    IdentityMismatch,
    SpawnAmbiguous,
    Cancel(CancellationPoint),
    CleanupUncertain(CleanupFault),
    ExternalRelease,
    DropWithoutWait(RetentionRetryFault),
    ReceiptSecondInspection(SecondInspectionRelease),
    RepeatRetry(RetryFaultPoint, RetryOrigin),
    SupervisorRetry(RetryFaultPoint),
    Legacy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AppleCommandExternalPlan {
    Normal,
    RegistryUnavailable,
    Image(QualificationFault),
    SpawnAmbiguous,
    Cancel(CancellationPoint),
    CleanupUncertain(CleanupFault),
    ExternalRelease,
    DropWithoutWait(RetentionRetryFault),
    ReceiptSecondInspection,
    RepeatRetry(RetryFaultPoint),
    SupervisorRetry(RetryFaultPoint),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TestDisposition {
    Completed,
    Rejected,
    Cancelled,
    Retained,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CleanupStep {
    Inspect,
    Stop,
    Reap,
    Remove,
    ProveAbsent,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CleanupIdentitySnapshot {
    pub provider: String,
    pub runtime_id: String,
    pub exact_name: String,
    pub token_digest: String,
    pub immutable_image_id: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PlannedLaunchIdentitySnapshot {
    pub provider: String,
    pub exact_name: String,
    pub token_digest: String,
    pub immutable_image_id: String,
    pub created_not_before: chrono::DateTime<chrono::Utc>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProductionCall {
    AgentEngineResolve,
    AgentEngineBuildOptions,
    ContainerOptionsResolve,
    RuntimeBuild,
    RunWithFrontend,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RetryObservation {
    Resolved,
    Retained,
}

#[derive(Default)]
struct AppleCommandEffectState {
    second_inspection_waiting: bool,
    second_inspection_release: Option<SecondInspectionRelease>,
    retry_fault_active: bool,
}

#[derive(Clone)]
pub(crate) struct AppleCommandEffects {
    state: Arc<(Mutex<AppleCommandEffectState>, Condvar)>,
}

impl AppleCommandEffects {
    fn new() -> Self {
        Self {
            state: Arc::new((
                Mutex::new(AppleCommandEffectState {
                    retry_fault_active: true,
                    ..AppleCommandEffectState::default()
                }),
                Condvar::new(),
            )),
        }
    }

    fn wait_for_second_inspection(&self, deadline: Instant) -> bool {
        let mut state = self.state.0.lock().unwrap_or_else(|p| p.into_inner());
        while !state.second_inspection_waiting {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            let (next, timeout) = self
                .state
                .1
                .wait_timeout(state, remaining)
                .unwrap_or_else(|p| p.into_inner());
            state = next;
            if timeout.timed_out() {
                return false;
            }
        }
        true
    }

    fn release_second_inspection(&self, release: SecondInspectionRelease) {
        let mut state = self.state.0.lock().unwrap_or_else(|p| p.into_inner());
        state.second_inspection_release = Some(release);
        self.state.1.notify_all();
    }

    pub(crate) fn wait_at_second_inspection(&self, deadline: Instant) -> SecondInspectionRelease {
        let mut state = self.state.0.lock().unwrap_or_else(|p| p.into_inner());
        state.second_inspection_waiting = true;
        self.state.1.notify_all();
        while state.second_inspection_release.is_none() {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return SecondInspectionRelease::Unavailable;
            };
            let (next, timeout) = self
                .state
                .1
                .wait_timeout(state, remaining)
                .unwrap_or_else(|p| p.into_inner());
            state = next;
            if timeout.timed_out() {
                return SecondInspectionRelease::Unavailable;
            }
        }
        state
            .second_inspection_release
            .unwrap_or(SecondInspectionRelease::Unavailable)
    }

    fn clear_retry_fault(&self) {
        self.state
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retry_fault_active = false;
        self.state.1.notify_all();
    }

    pub(crate) fn retry_fault_is_active(&self) -> bool {
        self.state
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retry_fault_active
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExternalReleaseBoundary {
    BeforeRunWithFrontend,
    ExecutionCreated,
    ProtectedReadyObserved,
    ProtectedReceiptRead,
    PreReleaseProviderEntriesCaptured,
    ReleaseAuthorized,
    ExecutionCompleted,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MountObservation {
    pub container_path: String,
    pub writable: bool,
    pub source_revalidated: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct OwnershipObservation {
    pub exact_plan: bool,
    pub child: bool,
    pub container: bool,
    pub controls: bool,
    pub staged_files: bool,
    pub registry_ticket: bool,
    pub quiescence_proven: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AppleCommandReport {
    /// Recorded by the test driver as it invokes the real public methods.
    /// Replaceable boundary callbacks cannot add entries.
    pub production_calls: Vec<ProductionCall>,
    pub disposition: TestDisposition,
    pub image_inspections: usize,
    pub gated_provider_calls: usize,
    pub spawn_attempts: usize,
    pub child_starts: usize,
    pub provider_entries_before_release: usize,
    pub provider_entries_total: usize,
    pub protected_ready_transitions: usize,
    pub accepted_releases: usize,
    pub writable_mounts: Vec<MountObservation>,
    pub argv: Vec<String>,
    pub container_name: Option<String>,
    pub reserved_label_values: Vec<String>,
    pub expected_reserved_label: Option<String>,
    pub trusted_image: bool,
    pub runtime_user: Option<String>,
    pub registry_arc_reached_spawn: bool,
    pub staged_request: Vec<u8>,
    pub expected_request: Vec<u8>,
    pub staged_manifests: BTreeMap<String, Vec<u8>>,
    pub expected_manifests: BTreeMap<String, Vec<u8>>,
    pub mutated_source_bytes_reached_guest: bool,
    pub second_spawn_attempted: bool,
    pub name_only_cleanup_attempted: bool,
    pub cleanup_steps: Vec<CleanupStep>,
    pub cleanup_inspection_identities: Vec<CleanupIdentitySnapshot>,
    pub stop_identities: Vec<CleanupIdentitySnapshot>,
    pub remove_identities: Vec<CleanupIdentitySnapshot>,
    pub planned_launch_identity: Option<PlannedLaunchIdentitySnapshot>,
    pub protected_launch_identity: Option<CleanupIdentitySnapshot>,
    pub protected_receipt_runtime_id: Option<String>,
    pub protected_receipt_container_name: Option<String>,
    pub cleanup_retained_ticket: Option<LaunchRetentionTicket>,
    pub cleanup_retained_ticket_present: Option<bool>,
    pub cleanup_active_resource_loans: Option<usize>,
    pub cleanup_resource_loan_releases: Option<usize>,
    pub ownership: OwnershipObservation,
    pub legacy_orchestrated_plan: bool,
    pub protected_receipt_observed: bool,
    pub release_absent_at_protected_receipt: bool,
    pub released_acknowledgments: usize,
    pub actual_provider_entries_before_release: usize,
    pub actual_provider_entries_total: usize,
    pub drop_elapsed: Option<Duration>,
    pub retained_ticket_before_retry: bool,
    pub retained_ticket_after_retry: bool,
    pub active_resource_loans_before_retry: usize,
    pub active_resource_loans_after_retry: usize,
    pub resource_loan_releases_before_retry: usize,
    pub resource_loan_releases_after_retry: usize,
    pub retry_observation: Option<RetryObservation>,
    pub receipt_exists_while_second_inspection_blocked: Option<bool>,
    pub retry_tickets: Vec<LaunchRetentionTicket>,
    pub retry_observations: Vec<RetryObservation>,
    pub retained_after_retry_attempts: Vec<bool>,
    pub active_resource_loans_after_retry_attempts: Vec<usize>,
    pub resource_loan_releases_after_retry_attempts: Vec<usize>,
    pub cleanup_steps_after_retry_attempts: Vec<Vec<CleanupStep>>,
    pub receipt_ever_observed_after_second_inspection: Option<bool>,
    pub receipt_exists_after_execution: Option<bool>,
    pub release_exists_after_execution: Option<bool>,
}

#[derive(Clone, Debug, Default)]
struct DriverEffects {
    protected_receipt_observed: bool,
    release_absent_at_protected_receipt: bool,
    released_acknowledgments: usize,
    actual_provider_entries_before_release: usize,
    actual_provider_entries_total: usize,
    drop_elapsed: Option<Duration>,
    protected_receipt_runtime_id: Option<String>,
    protected_receipt_container_name: Option<String>,
    cleanup_retained_ticket: Option<LaunchRetentionTicket>,
    cleanup_retained_ticket_present: Option<bool>,
    cleanup_active_resource_loans: Option<usize>,
    cleanup_resource_loan_releases: Option<usize>,
    retained_ticket_before_retry: bool,
    retained_ticket_after_retry: bool,
    active_resource_loans_before_retry: usize,
    active_resource_loans_after_retry: usize,
    resource_loan_releases_before_retry: usize,
    resource_loan_releases_after_retry: usize,
    retry_observation: Option<RetryObservation>,
    receipt_exists_while_second_inspection_blocked: Option<bool>,
    retry_tickets: Vec<LaunchRetentionTicket>,
    retry_observations: Vec<RetryObservation>,
    retained_after_retry_attempts: Vec<bool>,
    active_resource_loans_after_retry_attempts: Vec<usize>,
    resource_loan_releases_after_retry_attempts: Vec<usize>,
    cleanup_steps_after_retry_attempts: Vec<Vec<CleanupStep>>,
    receipt_ever_observed_after_second_inspection: Option<bool>,
    receipt_exists_after_execution: Option<bool>,
    release_exists_after_execution: Option<bool>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct AppleCommandObservation {
    image_inspections: usize,
    gated_provider_calls: usize,
    spawn_attempts: usize,
    child_starts: usize,
    provider_entries_before_release: usize,
    provider_entries_total: usize,
    protected_ready_transitions: usize,
    accepted_releases: usize,
    writable_mounts: Vec<MountObservation>,
    argv: Vec<String>,
    container_name: Option<String>,
    reserved_label_values: Vec<String>,
    trusted_image: bool,
    runtime_user: Option<String>,
    registry_arc_reached_spawn: bool,
    staged_request: Vec<u8>,
    staged_manifests: BTreeMap<String, Vec<u8>>,
    mutated_source_bytes_reached_guest: bool,
    second_spawn_attempted: bool,
    name_only_cleanup_attempted: bool,
    cleanup_steps: Vec<CleanupStep>,
    cleanup_inspection_identities: Vec<CleanupIdentitySnapshot>,
    stop_identities: Vec<CleanupIdentitySnapshot>,
    remove_identities: Vec<CleanupIdentitySnapshot>,
    planned_launch_identity: Option<PlannedLaunchIdentitySnapshot>,
    protected_launch_identity: Option<CleanupIdentitySnapshot>,
    ownership: OwnershipObservation,
    legacy_orchestrated_plan: bool,
    terminal_observed: bool,
    cancel_point: Option<CancellationPoint>,
}

#[derive(Clone)]
pub(crate) struct AppleCommandObserver {
    state: Arc<(Mutex<AppleCommandObservation>, Condvar)>,
}

impl AppleCommandObserver {
    fn new() -> Self {
        Self {
            state: Arc::new((
                Mutex::new(AppleCommandObservation::default()),
                Condvar::new(),
            )),
        }
    }

    pub(crate) fn observe_image(&self, trusted: bool, runtime_user: Option<String>) {
        let mut state = self.state.0.lock().unwrap_or_else(|p| p.into_inner());
        state.image_inspections += 1;
        state.trusted_image = trusted;
        state.runtime_user = runtime_user;
    }

    pub(crate) fn observe_spawn(
        &self,
        argv: Vec<String>,
        container_name: String,
        labels: Vec<String>,
        mounts: Vec<MountObservation>,
        registry_arc_reached_spawn: bool,
    ) {
        let mut state = self.state.0.lock().unwrap_or_else(|p| p.into_inner());
        state.spawn_attempts += 1;
        state.argv = argv;
        state.container_name = Some(container_name);
        state.reserved_label_values = labels;
        state.writable_mounts = mounts;
        state.registry_arc_reached_spawn = registry_arc_reached_spawn;
    }

    pub(crate) fn observe_child_start(&self) {
        self.state
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .child_starts += 1;
    }

    pub(crate) fn observe_provider_call(&self, step: Option<CleanupStep>) {
        let mut state = self.state.0.lock().unwrap_or_else(|p| p.into_inner());
        state.gated_provider_calls += 1;
        if let Some(step) = step {
            state.cleanup_steps.push(step);
        }
        self.state.1.notify_all();
    }

    pub(crate) fn observe_cleanup_identity(
        &self,
        step: CleanupStep,
        identity: CleanupIdentitySnapshot,
    ) {
        let mut state = self.state.0.lock().unwrap_or_else(|p| p.into_inner());
        match step {
            CleanupStep::Inspect => state.cleanup_inspection_identities.push(identity),
            CleanupStep::Stop => state.stop_identities.push(identity),
            CleanupStep::Remove => state.remove_identities.push(identity),
            CleanupStep::Reap | CleanupStep::ProveAbsent => {
                panic!("cleanup identity recorded for a non-provider-identity step")
            }
        }
    }

    pub(crate) fn observe_planned_launch_identity(&self, identity: PlannedLaunchIdentitySnapshot) {
        self.state
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .planned_launch_identity = Some(identity);
    }

    pub(crate) fn observe_protected_launch_identity(&self, identity: CleanupIdentitySnapshot) {
        self.state
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .protected_launch_identity = Some(identity);
    }

    pub(crate) fn observe_provider_entry(&self, before_release: bool) {
        let mut state = self.state.0.lock().unwrap_or_else(|p| p.into_inner());
        state.provider_entries_total += 1;
        if before_release {
            state.provider_entries_before_release += 1;
        }
    }

    pub(crate) fn observe_staged_snapshot(
        &self,
        staged_request: Vec<u8>,
        staged_manifests: BTreeMap<String, Vec<u8>>,
        mutated_source_bytes_reached_guest: bool,
    ) {
        let mut state = self.state.0.lock().unwrap_or_else(|p| p.into_inner());
        state.staged_request = staged_request;
        state.staged_manifests = staged_manifests;
        state.mutated_source_bytes_reached_guest = mutated_source_bytes_reached_guest;
    }

    pub(crate) fn observe_protected_ready(&self) {
        let mut state = self.state.0.lock().unwrap_or_else(|p| p.into_inner());
        state.protected_ready_transitions += 1;
        self.state.1.notify_all();
    }

    pub(crate) fn observe_accepted_release(&self) {
        self.state
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .accepted_releases += 1;
    }

    pub(crate) fn observe_cancel_point(&self, point: CancellationPoint) {
        let mut state = self.state.0.lock().unwrap_or_else(|p| p.into_inner());
        state.cancel_point = Some(point);
        self.state.1.notify_all();
    }

    fn wait_for_cancel_point(&self, expected: CancellationPoint, deadline: Instant) -> bool {
        let mut state = self.state.0.lock().unwrap_or_else(|p| p.into_inner());
        while state.cancel_point != Some(expected) {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            let (next, timeout) = self
                .state
                .1
                .wait_timeout(state, remaining)
                .unwrap_or_else(|p| p.into_inner());
            state = next;
            if timeout.timed_out() {
                return false;
            }
        }
        true
    }

    fn wait_for_protected_ready(&self, deadline: Instant) -> bool {
        let mut state = self.state.0.lock().unwrap_or_else(|p| p.into_inner());
        while state.protected_ready_transitions == 0 {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            let (next, timeout) = self
                .state
                .1
                .wait_timeout(state, remaining)
                .unwrap_or_else(|p| p.into_inner());
            state = next;
            if timeout.timed_out() {
                return false;
            }
        }
        true
    }

    pub(crate) fn observe_custody(
        &self,
        ownership: OwnershipObservation,
        second_spawn_attempted: bool,
        name_only_cleanup_attempted: bool,
        legacy_orchestrated_plan: bool,
    ) {
        let mut state = self.state.0.lock().unwrap_or_else(|p| p.into_inner());
        state.terminal_observed = true;
        state.ownership = ownership;
        state.second_spawn_attempted = second_spawn_attempted;
        state.name_only_cleanup_attempted = name_only_cleanup_attempted;
        state.legacy_orchestrated_plan = legacy_orchestrated_plan;
        self.state.1.notify_all();
    }

    fn snapshot_until(&self, deadline: Instant) -> AppleCommandObservation {
        let mut state = self.state.0.lock().unwrap_or_else(|p| p.into_inner());
        while !state.terminal_observed {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            let (next, timeout) = self
                .state
                .1
                .wait_timeout(state, remaining)
                .unwrap_or_else(|p| p.into_inner());
            state = next;
            if timeout.timed_out() {
                break;
            }
        }
        state.clone()
    }

    fn cleanup_steps(&self) -> Vec<CleanupStep> {
        self.state
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .cleanup_steps
            .clone()
    }

    fn wait_for_cleanup_steps(&self, minimum: usize, deadline: Instant) -> bool {
        let mut state = self.state.0.lock().unwrap_or_else(|p| p.into_inner());
        while state.cleanup_steps.len() < minimum {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            let (next, timeout) = self
                .state
                .1
                .wait_timeout(state, remaining)
                .unwrap_or_else(|p| p.into_inner());
            state = next;
            if timeout.timed_out() {
                return false;
            }
        }
        true
    }
}

struct GateFixture {
    _root: tempfile::TempDir,
    parent: PathBuf,
    guest: PathBuf,
    request_bytes: Vec<u8>,
    manifest_bytes: Vec<u8>,
}

struct ReceiptPublicationWatch {
    stop: Arc<AtomicBool>,
    observed: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl ReceiptPublicationWatch {
    fn finish(mut self) -> Result<bool, Box<dyn Error>> {
        self.stop.store(true, Ordering::Release);
        let worker = self
            .worker
            .take()
            .ok_or("receipt publication watcher worker unavailable")?;
        worker
            .join()
            .map_err(|_| "receipt publication watcher panicked")?;
        if let Some(error) = self.error.lock().unwrap_or_else(|p| p.into_inner()).take() {
            return Err(error.into());
        }
        Ok(self.observed.load(Ordering::Acquire))
    }
}

impl Drop for ReceiptPublicationWatch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl GateFixture {
    fn new(legacy: bool) -> Result<Self, Box<dyn Error>> {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::tempdir()?;
        let parent = root.path().join("control-parent");
        let guest = parent.join("guest-control");
        std::fs::create_dir(&parent)?;
        std::fs::create_dir(&guest)?;
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700))?;
        std::fs::set_permissions(&guest, std::fs::Permissions::from_mode(0o700))?;

        let manifest_bytes = b"{\"version\":1,\"entries\":[]}\n".to_vec();
        let manifest_digest = lower_hex(&Sha256::digest(&manifest_bytes));
        let request_bytes = format!(
            "{{\"version\":1,\"bindings\":[{{\"id\":\"workspace\",\"workspace_path\":\"/workspace\",\"manifest_id\":\"{manifest_digest}\",\"manifest_file\":\"workspace.manifest.json\",\"access\":\"read-only\"}}]}}\n"
        )
        .into_bytes();
        write_private(&parent.join(MANIFEST_NAME), &manifest_bytes)?;
        write_private(&parent.join("request.json"), &request_bytes)?;
        if !legacy {
            let token = "11".repeat(32);
            write_private(
                &parent.join("launch-intent.json"),
                format!(
                    "{{\"version\":1,\"container_name\":\"{NAME}\",\"launch_token\":\"{token}\"}}\n"
                )
                .as_bytes(),
            )?;
        }
        Ok(Self {
            _root: root,
            parent,
            guest,
            request_bytes,
            manifest_bytes,
        })
    }

    fn load(&self) -> Result<StartupGateSpec, Box<dyn Error>> {
        Ok(load_startup_gate(&self.parent, Duration::from_secs(30))?)
    }

    fn mutate_after_capture(&self, mutation: SealedMutation) -> Result<(), Box<dyn Error>> {
        match mutation {
            SealedMutation::Request => write_private(
                &self.parent.join("request.json"),
                b"{\"version\":1,\"bindings\":[]}\n",
            )?,
            SealedMutation::Manifest => write_private(
                &self.parent.join(MANIFEST_NAME),
                b"{\"version\":1,\"entries\":[{\"mutated\":true}]}\n",
            )?,
        }
        Ok(())
    }

    fn replace_guest_control(&self) -> Result<(), Box<dyn Error>> {
        let moved = self.parent.join("original-guest-control");
        std::fs::rename(&self.guest, moved)?;
        std::fs::create_dir(&self.guest)?;
        Ok(())
    }

    fn wait_for_protected_receipt(&self, deadline: Instant) -> Result<Vec<u8>, Box<dyn Error>> {
        let path = self.parent.join("launch.json");
        while Instant::now() < deadline {
            match std::fs::read(&path) {
                Ok(bytes) => return Ok(bytes),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error.into()),
            }
        }
        Err("protected launch receipt was not published before the fixture deadline".into())
    }

    fn release_exists(&self) -> bool {
        self.guest.join("release.json").exists()
    }

    fn receipt_exists(&self) -> bool {
        self.parent.join("launch.json").is_file()
    }

    fn watch_receipt_publication(&self) -> Result<ReceiptPublicationWatch, Box<dyn Error>> {
        let parent = self.parent.clone();
        let receipt = parent.join("launch.json");
        let stop = Arc::new(AtomicBool::new(false));
        let observed = Arc::new(AtomicBool::new(false));
        let error = Arc::new(Mutex::new(None));
        let worker_stop = Arc::clone(&stop);
        let worker_observed = Arc::clone(&observed);
        let worker_error = Arc::clone(&error);
        let worker = std::thread::Builder::new()
            .name("awman-receipt-publication-watch".into())
            .spawn(move || loop {
                let parent_ok = match std::fs::metadata(&parent) {
                    Ok(metadata) if metadata.is_dir() => true,
                    Ok(_) => {
                        *worker_error.lock().unwrap_or_else(|p| p.into_inner()) =
                            Some("protected control parent stopped being a directory".into());
                        false
                    }
                    Err(error) => {
                        *worker_error.lock().unwrap_or_else(|p| p.into_inner()) = Some(format!(
                            "protected control parent became unavailable: {error}"
                        ));
                        false
                    }
                };
                if !parent_ok {
                    return;
                }
                match std::fs::symlink_metadata(&receipt) {
                    Ok(metadata) if metadata.file_type().is_file() => {
                        worker_observed.store(true, Ordering::Release);
                    }
                    Ok(_) => {
                        *worker_error.lock().unwrap_or_else(|p| p.into_inner()) =
                            Some("protected launch receipt was not a regular file".into());
                        return;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        *worker_error.lock().unwrap_or_else(|p| p.into_inner()) = Some(format!(
                            "protected launch receipt observation failed: {error}"
                        ));
                        return;
                    }
                }
                if worker_stop.load(Ordering::Acquire) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(2));
            })?;
        Ok(ReceiptPublicationWatch {
            stop,
            observed,
            error,
            worker: Some(worker),
        })
    }

    fn final_record_presence(&self) -> Result<(bool, bool), Box<dyn Error>> {
        require_directory(&self.parent, "protected control parent")?;
        require_directory(&self.guest, "guest control directory")?;
        Ok((
            regular_file_presence(&self.parent.join("launch.json"))?,
            regular_file_presence(&self.guest.join("release.json"))?,
        ))
    }

    fn authorize_from_receipt(&self, receipt: &[u8]) -> Result<(), Box<dyn Error>> {
        let receipt: serde_json::Value = serde_json::from_slice(receipt)?;
        let nonce = receipt
            .get("nonce")
            .and_then(serde_json::Value::as_str)
            .filter(|nonce| {
                nonce.len() == 64
                    && nonce
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            })
            .ok_or("protected launch receipt did not contain a valid nonce")?;
        let release = serde_json::to_vec(&serde_json::json!({
            "nonce": nonce,
            "version": 1,
        }))?;
        write_private(&self.guest.join("release.json"), &release)?;
        Ok(())
    }

    fn released_acknowledgments(&self) -> usize {
        usize::from(self.guest.join(".released").is_file())
    }

    fn actual_provider_entries(&self) -> Result<usize, Box<dyn Error>> {
        match std::fs::read(self.guest.join(PROVIDER_ENTRY_MARKER)) {
            Ok(bytes) => Ok(bytes
                .split(|byte| *byte == b'\n')
                .filter(|record| *record == b"entered")
                .count()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(error) => Err(error.into()),
        }
    }
}

fn require_directory(path: &Path, description: &str) -> Result<(), Box<dyn Error>> {
    let metadata = std::fs::metadata(path)?;
    if metadata.is_dir() {
        Ok(())
    } else {
        Err(format!("{description} is not a directory").into())
    }
}

fn regular_file_presence(path: &Path) -> Result<bool, Box<dyn Error>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(true),
        Ok(_) => Err(format!("{} is not a regular file", path.display()).into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt as _;

    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn lower_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn external_plan(scenario: AppleCommandScenario) -> AppleCommandExternalPlan {
    match scenario {
        AppleCommandScenario::Qualification(QualificationFault::MissingRetentionRegistry) => {
            AppleCommandExternalPlan::RegistryUnavailable
        }
        AppleCommandScenario::Qualification(fault) if fault.image_inspections() == 1 => {
            AppleCommandExternalPlan::Image(fault)
        }
        AppleCommandScenario::SpawnAmbiguous => AppleCommandExternalPlan::SpawnAmbiguous,
        AppleCommandScenario::Cancel(point) => AppleCommandExternalPlan::Cancel(point),
        AppleCommandScenario::CleanupUncertain(fault) => {
            AppleCommandExternalPlan::CleanupUncertain(fault)
        }
        AppleCommandScenario::ExternalRelease => AppleCommandExternalPlan::ExternalRelease,
        AppleCommandScenario::DropWithoutWait(fault) => {
            AppleCommandExternalPlan::DropWithoutWait(fault)
        }
        AppleCommandScenario::ReceiptSecondInspection(_) => {
            AppleCommandExternalPlan::ReceiptSecondInspection
        }
        AppleCommandScenario::RepeatRetry(fault, _) => AppleCommandExternalPlan::RepeatRetry(fault),
        AppleCommandScenario::SupervisorRetry(fault) => {
            AppleCommandExternalPlan::SupervisorRetry(fault)
        }
        _ => AppleCommandExternalPlan::Normal,
    }
}

fn needs_external_release(scenario: AppleCommandScenario) -> bool {
    matches!(
        scenario,
        AppleCommandScenario::Qualified
            | AppleCommandScenario::SealedSourceMutated(_)
            | AppleCommandScenario::CleanupUncertain(_)
            | AppleCommandScenario::ExternalRelease
            | AppleCommandScenario::RepeatRetry(_, RetryOrigin::NormalWait)
    )
}

struct NullFrontend;

impl UserMessageSink for NullFrontend {
    fn write_message(&mut self, _message: UserMessage) {}
    fn replay_queued(&mut self) {}
}

#[async_trait::async_trait]
impl AgentFrontend for NullFrontend {
    fn report_status(&mut self, _status: AgentStatus) {}
    fn report_progress(&mut self, _progress: AgentProgress) {}

    fn take_io(&mut self) -> AgentIo {
        let (stdout, _) = tokio::sync::mpsc::unbounded_channel();
        let (stderr, _) = tokio::sync::mpsc::unbounded_channel();
        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
        AgentIo {
            stdout,
            stderr,
            stdin_tx,
            stdin_rx,
            resize: None,
            initial_size: None,
        }
    }
}

fn report_from_observation(
    observation: AppleCommandObservation,
    production_calls: Vec<ProductionCall>,
    expected_reserved_label: Option<String>,
    expected_request: Vec<u8>,
    expected_manifests: BTreeMap<String, Vec<u8>>,
    fallback: TestDisposition,
    effects: DriverEffects,
) -> AppleCommandReport {
    AppleCommandReport {
        production_calls,
        disposition: fallback,
        image_inspections: observation.image_inspections,
        gated_provider_calls: observation.gated_provider_calls,
        spawn_attempts: observation.spawn_attempts,
        child_starts: observation.child_starts,
        provider_entries_before_release: observation.provider_entries_before_release,
        provider_entries_total: observation.provider_entries_total,
        protected_ready_transitions: observation.protected_ready_transitions,
        accepted_releases: observation.accepted_releases,
        writable_mounts: observation.writable_mounts,
        argv: observation.argv,
        container_name: observation.container_name,
        reserved_label_values: observation.reserved_label_values,
        expected_reserved_label,
        trusted_image: observation.trusted_image,
        runtime_user: observation.runtime_user,
        registry_arc_reached_spawn: observation.registry_arc_reached_spawn,
        staged_request: observation.staged_request,
        expected_request,
        staged_manifests: observation.staged_manifests,
        expected_manifests,
        mutated_source_bytes_reached_guest: observation.mutated_source_bytes_reached_guest,
        second_spawn_attempted: observation.second_spawn_attempted,
        name_only_cleanup_attempted: observation.name_only_cleanup_attempted,
        cleanup_steps: observation.cleanup_steps,
        cleanup_inspection_identities: observation.cleanup_inspection_identities,
        stop_identities: observation.stop_identities,
        remove_identities: observation.remove_identities,
        planned_launch_identity: observation.planned_launch_identity,
        protected_launch_identity: observation.protected_launch_identity,
        protected_receipt_runtime_id: effects.protected_receipt_runtime_id,
        protected_receipt_container_name: effects.protected_receipt_container_name,
        cleanup_retained_ticket: effects.cleanup_retained_ticket,
        cleanup_retained_ticket_present: effects.cleanup_retained_ticket_present,
        cleanup_active_resource_loans: effects.cleanup_active_resource_loans,
        cleanup_resource_loan_releases: effects.cleanup_resource_loan_releases,
        ownership: observation.ownership,
        legacy_orchestrated_plan: observation.legacy_orchestrated_plan,
        protected_receipt_observed: effects.protected_receipt_observed,
        release_absent_at_protected_receipt: effects.release_absent_at_protected_receipt,
        released_acknowledgments: effects.released_acknowledgments,
        actual_provider_entries_before_release: effects.actual_provider_entries_before_release,
        actual_provider_entries_total: effects.actual_provider_entries_total,
        drop_elapsed: effects.drop_elapsed,
        retained_ticket_before_retry: effects.retained_ticket_before_retry,
        retained_ticket_after_retry: effects.retained_ticket_after_retry,
        active_resource_loans_before_retry: effects.active_resource_loans_before_retry,
        active_resource_loans_after_retry: effects.active_resource_loans_after_retry,
        resource_loan_releases_before_retry: effects.resource_loan_releases_before_retry,
        resource_loan_releases_after_retry: effects.resource_loan_releases_after_retry,
        retry_observation: effects.retry_observation,
        receipt_exists_while_second_inspection_blocked: effects
            .receipt_exists_while_second_inspection_blocked,
        retry_tickets: effects.retry_tickets,
        retry_observations: effects.retry_observations,
        retained_after_retry_attempts: effects.retained_after_retry_attempts,
        active_resource_loans_after_retry_attempts: effects
            .active_resource_loans_after_retry_attempts,
        resource_loan_releases_after_retry_attempts: effects
            .resource_loan_releases_after_retry_attempts,
        cleanup_steps_after_retry_attempts: effects.cleanup_steps_after_retry_attempts,
        receipt_ever_observed_after_second_inspection: effects
            .receipt_ever_observed_after_second_inspection,
        receipt_exists_after_execution: effects.receipt_exists_after_execution,
        release_exists_after_execution: effects.release_exists_after_execution,
    }
}

pub(crate) fn exercise(scenario: AppleCommandScenario) -> AppleCommandReport {
    exercise_public_path(scenario)
        .unwrap_or_else(|error| panic!("orchestrated Apple public-path fixture failed: {error}"))
}

fn exercise_public_path(
    scenario: AppleCommandScenario,
) -> Result<AppleCommandReport, Box<dyn Error>> {
    let fixture = GateFixture::new(matches!(scenario, AppleCommandScenario::Legacy))?;
    let expected_request = fixture.request_bytes.clone();
    let expected_manifests =
        BTreeMap::from([(MANIFEST_NAME.to_string(), fixture.manifest_bytes.clone())]);
    let gate = fixture.load()?;
    let expected_reserved_label = gate
        .control
        .orchestrated_parts()
        .map(|(_, identity)| lower_hex(&identity.token_digest));
    if let AppleCommandScenario::SealedSourceMutated(mutation) = scenario {
        fixture.mutate_after_capture(mutation)?;
    }
    if scenario == AppleCommandScenario::ControlReplaced {
        fixture.replace_guest_control()?;
    }

    let home = tempfile::tempdir()?;
    let repo = tempfile::tempdir()?;
    let resolver = StaticGitRootResolver::new(repo.path());
    let session = Session::open(
        repo.path().to_path_buf(),
        &resolver,
        SessionOpenOptions::default(),
    )?;
    let observer = AppleCommandObserver::new();
    let command_effects = AppleCommandEffects::new();
    let runtime = Arc::new(match scenario {
        AppleCommandScenario::Qualification(QualificationFault::DockerRuntime) => {
            ContainerRuntime::docker()
        }
        _ => ContainerRuntime::apple_with_orchestrated_command_test_boundary(
            external_plan(scenario),
            observer.clone(),
            command_effects.clone(),
        ),
    });
    let overlay = OverlayEngine::with_auth_resolver(
        crate::data::fs::auth_paths::AuthPathResolver::at_home(home.path()),
    )
    .with_secret_files_provider(Arc::new(|_| Vec::new()));
    let engine = AgentEngine::new(Arc::new(overlay), Arc::clone(&runtime));
    let sandbox_runtime =
        if scenario == AppleCommandScenario::Qualification(QualificationFault::SandboxRuntime) {
            Some(SandboxRuntime::dsbx()?)
        } else {
            None
        };
    let agent = AgentName::new(
        if scenario == AppleCommandScenario::Qualification(QualificationFault::Acp) {
            "cline"
        } else {
            "claude"
        },
    )?;
    let run = AgentRunOptions {
        startup_gate: Some(gate),
        launch_mode: if scenario == AppleCommandScenario::Qualification(QualificationFault::Acp) {
            LaunchMode::Acp
        } else {
            LaunchMode::Stdio
        },
        allow_docker: scenario
            == AppleCommandScenario::Qualification(QualificationFault::AllowDocker),
        non_interactive: true,
        ..Default::default()
    };
    let raw_option_path = matches!(
        scenario,
        AppleCommandScenario::Qualification(QualificationFault::CallerName)
            | AppleCommandScenario::Qualification(QualificationFault::DuplicateName)
            | AppleCommandScenario::Qualification(QualificationFault::ReservedLabel)
            | AppleCommandScenario::Qualification(QualificationFault::RemoveOnExit)
            | AppleCommandScenario::IdentityMismatch
    );
    let mut production_calls = vec![if raw_option_path {
        ProductionCall::AgentEngineBuildOptions
    } else {
        ProductionCall::AgentEngineResolve
    }];

    let resolved: Result<ResolvedAgentOptions, Box<dyn Error>> = if raw_option_path {
        let mut options = match engine.build_options_with_credentials(
            &session,
            &agent,
            &run,
            &AgentCredentials::default(),
        ) {
            Ok(options) => options,
            Err(_) => {
                return Ok(report_from_observation(
                    observer.snapshot_until(Instant::now()),
                    production_calls,
                    expected_reserved_label.clone(),
                    expected_request.clone(),
                    expected_manifests.clone(),
                    TestDisposition::Rejected,
                    DriverEffects::default(),
                ));
            }
        };
        match scenario {
            AppleCommandScenario::Qualification(QualificationFault::CallerName) => {
                options.push(ContainerOption::Name(ContainerName::new(
                    "caller-selected-name",
                )));
            }
            AppleCommandScenario::Qualification(QualificationFault::DuplicateName) => {
                options.push(ContainerOption::Name(ContainerName::new(NAME)));
                options.push(ContainerOption::Name(ContainerName::new(NAME)));
            }
            AppleCommandScenario::Qualification(QualificationFault::ReservedLabel) => {
                options.push(ContainerOption::Label {
                    key: RESERVED_LABEL.into(),
                    value: "caller-selected-value".into(),
                });
            }
            AppleCommandScenario::Qualification(QualificationFault::RemoveOnExit) => {
                options.retain(|option| !matches!(option, ContainerOption::KeepContainer));
            }
            AppleCommandScenario::IdentityMismatch => {
                options.push(ContainerOption::Name(ContainerName::new(
                    "identity-mismatch",
                )));
            }
            _ => {}
        }
        production_calls.push(ProductionCall::ContainerOptionsResolve);
        ResolvedAgentOptions::container(options).map_err(|error| error.into())
    } else {
        let option_runtime: &dyn AgentRuntimeEngine = sandbox_runtime
            .as_ref()
            .map_or(runtime.as_ref(), |sandbox| {
                sandbox as &dyn AgentRuntimeEngine
            });
        engine
            .resolve_agent_options(
                &session,
                &agent,
                &run,
                &AgentCredentials::default(),
                option_runtime,
            )
            .map_err(|error| error.into())
    };
    let resolved = match resolved {
        Ok(resolved) => resolved,
        Err(_) => {
            return Ok(report_from_observation(
                observer.snapshot_until(Instant::now()),
                production_calls,
                expected_reserved_label.clone(),
                expected_request.clone(),
                expected_manifests.clone(),
                TestDisposition::Rejected,
                DriverEffects::default(),
            ));
        }
    };
    if !production_calls.contains(&ProductionCall::ContainerOptionsResolve) {
        production_calls.push(ProductionCall::ContainerOptionsResolve);
    }

    if matches!(
        scenario,
        AppleCommandScenario::Qualification(QualificationFault::DockerRuntime)
            | AppleCommandScenario::Qualification(QualificationFault::SandboxRuntime)
    ) {
        return Err("unsupported runtime reached container build".into());
    }

    production_calls.push(ProductionCall::RuntimeBuild);
    let instance = match AgentRuntimeEngine::build(runtime.as_ref(), resolved) {
        Ok(instance) => instance,
        Err(_) => {
            let observation = observer.snapshot_until(Instant::now());
            return Ok(report_from_observation(
                observation,
                production_calls,
                expected_reserved_label.clone(),
                expected_request.clone(),
                expected_manifests.clone(),
                TestDisposition::Rejected,
                DriverEffects::default(),
            ));
        }
    };
    production_calls.push(ProductionCall::RunWithFrontend);

    let tokio = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let retry_registry = if matches!(
        scenario,
        AppleCommandScenario::SpawnAmbiguous
            | AppleCommandScenario::Cancel(_)
            | AppleCommandScenario::CleanupUncertain(_)
            | AppleCommandScenario::DropWithoutWait(_)
            | AppleCommandScenario::ReceiptSecondInspection(_)
            | AppleCommandScenario::RepeatRetry(_, _)
            | AppleCommandScenario::SupervisorRetry(_)
    ) {
        Some(runtime.launch_retention()?)
    } else {
        None
    };
    let retry_probe = retry_registry
        .as_deref()
        .map(retention_test_support::registry_probe);
    let _manual_supervisor_claim_barrier = match scenario {
        AppleCommandScenario::SupervisorRetry(_) => None,
        _ => retry_probe
            .as_ref()
            .map(retention_test_support::RetentionProbe::hold_supervisor_active_claims),
    };
    let mut effects = DriverEffects::default();
    let mut external_release_boundary = ExternalReleaseBoundary::BeforeRunWithFrontend;
    let wait_result: Result<(), crate::engine::error::EngineError> = tokio.block_on(async {
        let mut execution = instance.run_with_frontend(Box::new(NullFrontend))?;
        if scenario == AppleCommandScenario::ExternalRelease {
            external_release_boundary = ExternalReleaseBoundary::ExecutionCreated;
        }
        if let AppleCommandScenario::Cancel(point) = scenario {
            if !observer.wait_for_cancel_point(point, Instant::now() + Duration::from_secs(2)) {
                return Err(crate::engine::error::EngineError::Container(
                    "cancel fixture did not reach the requested production boundary".into(),
                ));
            }
            execution.cancel()?;
        }

        if let AppleCommandScenario::ReceiptSecondInspection(release) = scenario {
            if !command_effects.wait_for_second_inspection(Instant::now() + Duration::from_secs(2))
            {
                return Err(crate::engine::error::EngineError::Container(
                    "receipt fixture did not reach the second exact inspection".into(),
                ));
            }
            effects.receipt_exists_while_second_inspection_blocked = Some(fixture.receipt_exists());
            effects.release_absent_at_protected_receipt = !fixture.release_exists();
            effects.actual_provider_entries_before_release = fixture
                .actual_provider_entries()
                .map_err(|error| crate::engine::error::EngineError::Container(error.to_string()))?;
            let receipt_watch = fixture
                .watch_receipt_publication()
                .map_err(|error| crate::engine::error::EngineError::Container(error.to_string()))?;
            command_effects.release_second_inspection(release);

            if release == SecondInspectionRelease::Matching {
                let receipt = fixture
                    .wait_for_protected_receipt(Instant::now() + Duration::from_secs(2))
                    .map_err(|error| {
                        crate::engine::error::EngineError::Container(error.to_string())
                    })?;
                effects.protected_receipt_observed = !receipt.is_empty();
                fixture.authorize_from_receipt(&receipt).map_err(|error| {
                    crate::engine::error::EngineError::Container(error.to_string())
                })?;
            }
            let wait_result = execution.wait().await.map(|_| ());
            effects.receipt_ever_observed_after_second_inspection =
                Some(receipt_watch.finish().map_err(|error| {
                    crate::engine::error::EngineError::Container(error.to_string())
                })?);
            let (receipt_exists, release_exists) = fixture
                .final_record_presence()
                .map_err(|error| crate::engine::error::EngineError::Container(error.to_string()))?;
            effects.receipt_exists_after_execution = Some(receipt_exists);
            effects.release_exists_after_execution = Some(release_exists);
            return wait_result;
        }

        if let AppleCommandScenario::RepeatRetry(fault, origin) = scenario {
            if !observer.wait_for_protected_ready(Instant::now() + Duration::from_secs(2)) {
                return Err(crate::engine::error::EngineError::Container(
                    "repeat-retry fixture did not reach the protected receipt boundary".into(),
                ));
            }
            let receipt = fixture
                .wait_for_protected_receipt(Instant::now() + Duration::from_secs(2))
                .map_err(|error| crate::engine::error::EngineError::Container(error.to_string()))?;
            effects.protected_receipt_observed = !receipt.is_empty();
            effects.release_absent_at_protected_receipt = !fixture.release_exists();
            effects.actual_provider_entries_before_release = fixture
                .actual_provider_entries()
                .map_err(|error| crate::engine::error::EngineError::Container(error.to_string()))?;

            match origin {
                RetryOrigin::Drop => {
                    let drop_started = Instant::now();
                    drop(execution);
                    effects.drop_elapsed = Some(drop_started.elapsed());
                }
                RetryOrigin::NormalWait => {
                    if fault != RetryFaultPoint::FinalAbsence {
                        return Err(crate::engine::error::EngineError::Container(
                            "normal-wait repeat retry requires final-absence uncertainty".into(),
                        ));
                    }
                    fixture.authorize_from_receipt(&receipt).map_err(|error| {
                        crate::engine::error::EngineError::Container(error.to_string())
                    })?;
                    let _ = execution.wait().await;
                }
            }

            let registry = retry_registry.as_ref().ok_or_else(|| {
                crate::engine::error::EngineError::Container(
                    "repeat-retry registry unavailable".into(),
                )
            })?;
            let probe = retry_probe.as_ref().ok_or_else(|| {
                crate::engine::error::EngineError::Container(
                    "repeat-retry probe unavailable".into(),
                )
            })?;
            let ticket_deadline = Instant::now() + Duration::from_secs(2);
            let initial_ticket = loop {
                if let Some(ticket) = probe.only_ticket() {
                    break ticket;
                }
                if Instant::now() >= ticket_deadline {
                    return Err(crate::engine::error::EngineError::Container(
                        "repeat-retry fixture did not retain exactly one ticket".into(),
                    ));
                }
                std::thread::sleep(Duration::from_millis(10));
            };

            if origin == RetryOrigin::NormalWait {
                effects.retry_tickets.push(initial_ticket);
                effects.retry_observations.push(RetryObservation::Retained);
                effects
                    .retained_after_retry_attempts
                    .push(probe.contains(initial_ticket));
                effects
                    .active_resource_loans_after_retry_attempts
                    .push(probe.active_gated_resource_loans(initial_ticket));
                effects
                    .resource_loan_releases_after_retry_attempts
                    .push(probe.resource_loan_releases(initial_ticket));
                effects
                    .cleanup_steps_after_retry_attempts
                    .push(observer.cleanup_steps());
            } else {
                let first = registry
                    .retry_active_gated(initial_ticket, Instant::now() + Duration::from_secs(2))?;
                effects.retry_tickets.push(initial_ticket);
                effects.retry_observations.push(match first {
                    ActiveGatedRetryOutcome::Resolved => RetryObservation::Resolved,
                    ActiveGatedRetryOutcome::Retained => RetryObservation::Retained,
                });
                effects
                    .retained_after_retry_attempts
                    .push(probe.contains(initial_ticket));
                effects
                    .active_resource_loans_after_retry_attempts
                    .push(probe.active_gated_resource_loans(initial_ticket));
                effects
                    .resource_loan_releases_after_retry_attempts
                    .push(probe.resource_loan_releases(initial_ticket));
                effects
                    .cleanup_steps_after_retry_attempts
                    .push(observer.cleanup_steps());
            }

            command_effects.clear_retry_fault();
            let retry_ticket = probe.only_ticket().ok_or_else(|| {
                crate::engine::error::EngineError::Container(
                    "repeat-retry fixture lost the retained ticket before retry".into(),
                )
            })?;
            let second = registry
                .retry_active_gated(retry_ticket, Instant::now() + Duration::from_secs(2))?;
            effects.retry_tickets.push(retry_ticket);
            effects.retry_observations.push(match second {
                ActiveGatedRetryOutcome::Resolved => RetryObservation::Resolved,
                ActiveGatedRetryOutcome::Retained => RetryObservation::Retained,
            });
            effects
                .retained_after_retry_attempts
                .push(probe.contains(retry_ticket));
            effects
                .active_resource_loans_after_retry_attempts
                .push(probe.active_gated_resource_loans(retry_ticket));
            effects
                .resource_loan_releases_after_retry_attempts
                .push(probe.resource_loan_releases(retry_ticket));
            effects
                .cleanup_steps_after_retry_attempts
                .push(observer.cleanup_steps());
            effects.actual_provider_entries_total = fixture
                .actual_provider_entries()
                .map_err(|error| crate::engine::error::EngineError::Container(error.to_string()))?;
            return Ok(());
        }

        if let AppleCommandScenario::SupervisorRetry(RetryFaultPoint::FinalAbsence) = scenario {
            if !observer.wait_for_protected_ready(Instant::now() + Duration::from_secs(2)) {
                return Err(crate::engine::error::EngineError::Container(
                    "supervisor-retry fixture did not reach the protected receipt boundary".into(),
                ));
            }
            let receipt = fixture
                .wait_for_protected_receipt(Instant::now() + Duration::from_secs(2))
                .map_err(|error| crate::engine::error::EngineError::Container(error.to_string()))?;
            effects.protected_receipt_observed = !receipt.is_empty();
            effects.release_absent_at_protected_receipt = !fixture.release_exists();
            effects.actual_provider_entries_before_release = fixture
                .actual_provider_entries()
                .map_err(|error| crate::engine::error::EngineError::Container(error.to_string()))?;
            drop(execution);

            let probe = retry_probe.as_ref().ok_or_else(|| {
                crate::engine::error::EngineError::Container(
                    "supervisor-retry probe unavailable".into(),
                )
            })?;
            if !observer.wait_for_cleanup_steps(6, Instant::now() + Duration::from_secs(2)) {
                return Err(crate::engine::error::EngineError::Container(
                    "production supervisor did not retain final-absence uncertainty".into(),
                ));
            }
            let retained = probe
                .wait_until_only_active_gated_retained(Instant::now() + Duration::from_secs(2))
                .ok_or_else(|| {
                    crate::engine::error::EngineError::Container(
                        "production supervisor did not publish one retained active snapshot".into(),
                    )
                })?;
            effects.retry_tickets.push(retained.ticket);
            effects.retry_observations.push(RetryObservation::Retained);
            effects
                .retained_after_retry_attempts
                .push(retained.ticket_present && retained.entry_present);
            effects
                .active_resource_loans_after_retry_attempts
                .push(retained.resource_loans);
            effects
                .resource_loan_releases_after_retry_attempts
                .push(retained.resource_loan_releases);
            effects
                .cleanup_steps_after_retry_attempts
                .push(observer.cleanup_steps());

            command_effects.clear_retry_fault();
            probe.wake_supervisor();
            let resolved = probe
                .wait_until_observed_ticket_resolved(
                    retained,
                    Instant::now() + Duration::from_secs(2),
                )
                .ok_or_else(|| {
                    crate::engine::error::EngineError::Container(
                        "production supervisor did not resolve the observed active ticket".into(),
                    )
                })?;
            effects.retry_tickets.push(resolved.ticket);
            effects.retry_observations.push(RetryObservation::Resolved);
            effects
                .retained_after_retry_attempts
                .push(resolved.ticket_present || resolved.entry_present);
            effects
                .active_resource_loans_after_retry_attempts
                .push(resolved.resource_loans);
            effects
                .resource_loan_releases_after_retry_attempts
                .push(resolved.resource_loan_releases);
            effects
                .cleanup_steps_after_retry_attempts
                .push(observer.cleanup_steps());
            effects.actual_provider_entries_total = fixture
                .actual_provider_entries()
                .map_err(|error| crate::engine::error::EngineError::Container(error.to_string()))?;
            return Ok(());
        }

        if matches!(scenario, AppleCommandScenario::DropWithoutWait(_)) {
            if !observer.wait_for_protected_ready(Instant::now() + Duration::from_secs(2)) {
                return Err(crate::engine::error::EngineError::Container(
                    "drop fixture did not reach the protected receipt boundary".into(),
                ));
            }
            let receipt = fixture
                .wait_for_protected_receipt(Instant::now() + Duration::from_secs(2))
                .map_err(|error| crate::engine::error::EngineError::Container(error.to_string()))?;
            effects.protected_receipt_observed = !receipt.is_empty();
            effects.release_absent_at_protected_receipt = !fixture.release_exists();
            effects.actual_provider_entries_before_release = fixture
                .actual_provider_entries()
                .map_err(|error| crate::engine::error::EngineError::Container(error.to_string()))?;

            let drop_started = Instant::now();
            drop(execution);
            effects.drop_elapsed = Some(drop_started.elapsed());

            let registry = retry_registry.as_ref().ok_or_else(|| {
                crate::engine::error::EngineError::Container(
                    "drop fixture retention registry unavailable".into(),
                )
            })?;
            let probe = retry_probe.as_ref().ok_or_else(|| {
                crate::engine::error::EngineError::Container(
                    "drop fixture retention probe unavailable".into(),
                )
            })?;
            let ticket_deadline = Instant::now() + Duration::from_secs(2);
            let ticket = loop {
                if let Some(ticket) = probe.only_ticket() {
                    break ticket;
                }
                if Instant::now() >= ticket_deadline {
                    return Err(crate::engine::error::EngineError::Container(
                        "drop fixture did not retain exactly one ticket".into(),
                    ));
                }
                std::thread::sleep(Duration::from_millis(10));
            };
            effects.retained_ticket_before_retry = probe.contains(ticket);
            effects.active_resource_loans_before_retry = probe.active_gated_resource_loans(ticket);
            effects.resource_loan_releases_before_retry = probe.resource_loan_releases(ticket);

            let retry =
                registry.retry_active_gated(ticket, Instant::now() + Duration::from_secs(2))?;
            effects.retry_observation = Some(match retry {
                ActiveGatedRetryOutcome::Resolved => RetryObservation::Resolved,
                ActiveGatedRetryOutcome::Retained => RetryObservation::Retained,
            });
            effects.retained_ticket_after_retry = probe.contains(ticket);
            effects.active_resource_loans_after_retry = probe.active_gated_resource_loans(ticket);
            effects.resource_loan_releases_after_retry = probe.resource_loan_releases(ticket);
            effects.actual_provider_entries_total = fixture
                .actual_provider_entries()
                .map_err(|error| crate::engine::error::EngineError::Container(error.to_string()))?;
            return Ok(());
        }

        if needs_external_release(scenario) {
            if !observer.wait_for_protected_ready(Instant::now() + Duration::from_secs(2)) {
                return Err(crate::engine::error::EngineError::Container(
                    "external release fixture did not reach the protected receipt boundary".into(),
                ));
            }
            if scenario == AppleCommandScenario::ExternalRelease {
                external_release_boundary = ExternalReleaseBoundary::ProtectedReadyObserved;
            }
            let receipt = fixture
                .wait_for_protected_receipt(Instant::now() + Duration::from_secs(2))
                .map_err(|error| crate::engine::error::EngineError::Container(error.to_string()))?;
            if scenario == AppleCommandScenario::ExternalRelease {
                external_release_boundary = ExternalReleaseBoundary::ProtectedReceiptRead;
            }
            effects.protected_receipt_observed = !receipt.is_empty();
            let protected_receipt: serde_json::Value = serde_json::from_slice(&receipt)
                .map_err(|error| crate::engine::error::EngineError::Container(error.to_string()))?;
            effects.protected_receipt_runtime_id = protected_receipt
                .get("runtime_id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            effects.protected_receipt_container_name = protected_receipt
                .get("container_name")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            effects.release_absent_at_protected_receipt = !fixture.release_exists();
            effects.actual_provider_entries_before_release = fixture
                .actual_provider_entries()
                .map_err(|error| crate::engine::error::EngineError::Container(error.to_string()))?;
            if scenario == AppleCommandScenario::ExternalRelease {
                external_release_boundary =
                    ExternalReleaseBoundary::PreReleaseProviderEntriesCaptured;
            }
            fixture
                .authorize_from_receipt(&receipt)
                .map_err(|error| crate::engine::error::EngineError::Container(error.to_string()))?;
            if scenario == AppleCommandScenario::ExternalRelease {
                external_release_boundary = ExternalReleaseBoundary::ReleaseAuthorized;
            }
        }

        let execution_result = execution.wait().await.map(|_| ());
        if scenario == AppleCommandScenario::ExternalRelease && execution_result.is_ok() {
            external_release_boundary = ExternalReleaseBoundary::ExecutionCompleted;
        }

        execution_result
    });
    if matches!(
        scenario,
        AppleCommandScenario::CleanupUncertain(CleanupFault::PreRemoveRuntimeIdChanged)
            | AppleCommandScenario::CleanupUncertain(CleanupFault::PreRemoveCreatedAtChanged)
    ) {
        let probe = retry_probe
            .as_ref()
            .ok_or("cleanup-authority retention probe unavailable")?;
        if let Some(ticket) = probe.only_ticket() {
            effects.cleanup_retained_ticket = Some(ticket);
            effects.cleanup_retained_ticket_present = Some(probe.contains(ticket));
            effects.cleanup_active_resource_loans = Some(probe.active_gated_resource_loans(ticket));
            effects.cleanup_resource_loan_releases = Some(probe.resource_loan_releases(ticket));
        }
    }
    if scenario == AppleCommandScenario::ExternalRelease {
        if let Err(error) = &wait_result {
            const ERROR_LIMIT: usize = 512;
            const TRUNCATED: &str = "...[truncated]";
            let mut error = error.to_string();
            if error.len() > ERROR_LIMIT {
                let mut end = ERROR_LIMIT - TRUNCATED.len();
                while !error.is_char_boundary(end) {
                    end -= 1;
                }
                error.truncate(end);
                error.push_str(TRUNCATED);
            }
            return Err(format!(
                "external release driver failed: last_boundary={external_release_boundary:?}; error={error}"
            )
            .into());
        }
    }
    effects.released_acknowledgments = fixture.released_acknowledgments();
    effects.actual_provider_entries_total = fixture.actual_provider_entries()?;
    let observation = observer.snapshot_until(Instant::now() + Duration::from_secs(2));
    let fallback = match scenario {
        AppleCommandScenario::SpawnAmbiguous | AppleCommandScenario::CleanupUncertain(_)
            if observation.ownership.exact_plan =>
        {
            TestDisposition::Retained
        }
        AppleCommandScenario::Cancel(_) if observation.ownership.exact_plan => {
            TestDisposition::Retained
        }
        AppleCommandScenario::Cancel(_) => TestDisposition::Cancelled,
        AppleCommandScenario::DropWithoutWait(_) if effects.retained_ticket_before_retry => {
            TestDisposition::Retained
        }
        _ if wait_result.is_ok() => TestDisposition::Completed,
        _ => TestDisposition::Rejected,
    };
    Ok(report_from_observation(
        observation,
        production_calls,
        expected_reserved_label,
        expected_request,
        expected_manifests,
        fallback,
        effects,
    ))
}

fn assert_no_launch_effects(report: &AppleCommandReport) {
    assert_eq!(report.gated_provider_calls, 0);
    assert_eq!(report.spawn_attempts, 0);
    assert_eq!(report.child_starts, 0);
    assert_eq!(report.provider_entries_before_release, 0);
    assert_eq!(report.provider_entries_total, 0);
    assert!(report.writable_mounts.is_empty());
    assert!(report.cleanup_steps.is_empty());
    assert!(!report.name_only_cleanup_attempted);
}

fn assert_staged_snapshot_matches_fixture(report: &AppleCommandReport) {
    assert!(!report.expected_request.is_empty());
    assert_eq!(report.staged_request, report.expected_request);
    assert_eq!(report.expected_manifests.len(), 1);
    let expected_manifest = report
        .expected_manifests
        .get(MANIFEST_NAME)
        .expect("fixture must retain its declared manifest");
    assert!(!expected_manifest.is_empty());
    assert_eq!(report.staged_manifests, report.expected_manifests);
}

fn assert_production_prefix(report: &AppleCommandReport, expected: &[ProductionCall]) {
    assert!(
        report.production_calls.starts_with(expected),
        "public production path {:?} did not start with {expected:?}",
        report.production_calls
    );
}

fn assert_full_orchestrated_spawn_path(report: &AppleCommandReport) {
    assert_production_prefix(
        report,
        &[
            ProductionCall::AgentEngineResolve,
            ProductionCall::ContainerOptionsResolve,
            ProductionCall::RuntimeBuild,
            ProductionCall::RunWithFrontend,
        ],
    );
}

fn assert_exact_owned_retention(report: &AppleCommandReport) {
    assert_eq!(report.disposition, TestDisposition::Retained);
    assert!(report.ownership.exact_plan);
    assert!(report.ownership.child);
    assert!(report.ownership.container);
    assert!(report.ownership.controls);
    assert!(report.ownership.staged_files);
    assert!(report.ownership.registry_ticket);
    assert!(!report.name_only_cleanup_attempted);
    assert!(!report.second_spawn_attempted);
}

#[test]
fn qualification_rejects_at_the_earliest_authoritative_phase() {
    for fault in [
        QualificationFault::DockerRuntime,
        QualificationFault::SandboxRuntime,
        QualificationFault::Acp,
        QualificationFault::AllowDocker,
        QualificationFault::CallerName,
        QualificationFault::DuplicateName,
        QualificationFault::ReservedLabel,
        QualificationFault::RemoveOnExit,
        QualificationFault::MissingRetentionRegistry,
        QualificationFault::UntrustedImage,
        QualificationFault::ImageEntrypoint,
        QualificationFault::ImageLoaderEnvironment,
        QualificationFault::MissingRuntimeUser,
        QualificationFault::InvalidRuntimeUser,
    ] {
        let report = exercise(AppleCommandScenario::Qualification(fault));
        assert_eq!(
            report.disposition,
            TestDisposition::Rejected,
            "qualification case {fault:?} must reject"
        );
        assert_eq!(
            report.image_inspections,
            fault.image_inspections(),
            "qualification case {fault:?} crossed the wrong inspection phase"
        );
        let expected_path = match fault {
            QualificationFault::DockerRuntime
            | QualificationFault::SandboxRuntime
            | QualificationFault::Acp
            | QualificationFault::AllowDocker => vec![ProductionCall::AgentEngineResolve],
            QualificationFault::CallerName
            | QualificationFault::DuplicateName
            | QualificationFault::ReservedLabel
            | QualificationFault::RemoveOnExit => vec![
                ProductionCall::AgentEngineBuildOptions,
                ProductionCall::ContainerOptionsResolve,
            ],
            QualificationFault::MissingRetentionRegistry => vec![
                ProductionCall::AgentEngineResolve,
                ProductionCall::ContainerOptionsResolve,
                ProductionCall::RuntimeBuild,
            ],
            QualificationFault::UntrustedImage
            | QualificationFault::ImageEntrypoint
            | QualificationFault::ImageLoaderEnvironment
            | QualificationFault::MissingRuntimeUser
            | QualificationFault::InvalidRuntimeUser => vec![
                ProductionCall::AgentEngineResolve,
                ProductionCall::ContainerOptionsResolve,
                ProductionCall::RuntimeBuild,
            ],
        };
        assert_eq!(
            report.production_calls, expected_path,
            "qualification case {fault:?} did not reject at its authoritative production phase"
        );
        assert_no_launch_effects(&report);
    }
}

#[test]
fn qualified_command_path_propagates_one_verified_apple_authority() {
    let report = exercise(AppleCommandScenario::Qualified);

    assert_eq!(report.disposition, TestDisposition::Completed);
    assert_eq!(
        report.production_calls,
        vec![
            ProductionCall::AgentEngineResolve,
            ProductionCall::ContainerOptionsResolve,
            ProductionCall::RuntimeBuild,
            ProductionCall::RunWithFrontend,
        ]
    );
    assert_eq!(report.image_inspections, 1);
    assert_eq!(report.spawn_attempts, 1);
    assert_eq!(report.child_starts, 1);
    assert_eq!(report.container_name.as_deref(), Some(NAME));
    let expected_label = report
        .expected_reserved_label
        .clone()
        .expect("qualified fixture must carry the validated intent digest");
    assert_eq!(report.reserved_label_values, vec![expected_label.clone()]);
    assert!(report.trusted_image);
    assert_eq!(report.runtime_user.as_deref(), Some(RUNTIME_USER));
    assert!(report.registry_arc_reached_spawn);
    assert_staged_snapshot_matches_fixture(&report);
    assert_eq!(
        report.writable_mounts,
        vec![MountObservation {
            container_path: CONTROL_PATH.into(),
            writable: true,
            source_revalidated: true,
        }]
    );
    assert_eq!(report.provider_entries_before_release, 0);
    assert_eq!(report.protected_ready_transitions, 1);
    assert_eq!(report.accepted_releases, 1);
    assert_eq!(report.provider_entries_total, 1);
    assert!(!report.second_spawn_attempted);
    assert!(!report.name_only_cleanup_attempted);

    let name_values = report
        .argv
        .windows(2)
        .filter(|pair| pair[0] == "--name")
        .map(|pair| pair[1].as_str())
        .collect::<Vec<_>>();
    assert_eq!(name_values, vec![NAME]);
    let labels = report
        .argv
        .windows(2)
        .filter(|pair| pair[0] == "--label")
        .map(|pair| pair[1].as_str())
        .filter(|value| {
            value
                .strip_prefix(RESERVED_LABEL)
                .is_some_and(|suffix| suffix.starts_with('='))
        })
        .collect::<Vec<_>>();
    assert_eq!(labels, vec![format!("{RESERVED_LABEL}={expected_label}")]);
    assert!(!report.argv.iter().any(|arg| arg == "--rm"));
}

#[test]
fn control_replacement_and_sealed_source_mutation_never_change_guest_authority() {
    let replaced = exercise(AppleCommandScenario::ControlReplaced);
    assert_eq!(replaced.disposition, TestDisposition::Rejected);
    assert_production_prefix(
        &replaced,
        &[
            ProductionCall::AgentEngineResolve,
            ProductionCall::ContainerOptionsResolve,
            ProductionCall::RuntimeBuild,
            ProductionCall::RunWithFrontend,
        ],
    );
    assert_no_launch_effects(&replaced);
    assert!(replaced.ownership.controls || replaced.ownership.quiescence_proven);
    assert!(!replaced.mutated_source_bytes_reached_guest);

    for mutation in [SealedMutation::Request, SealedMutation::Manifest] {
        let report = exercise(AppleCommandScenario::SealedSourceMutated(mutation));
        assert_production_prefix(
            &report,
            &[
                ProductionCall::AgentEngineResolve,
                ProductionCall::ContainerOptionsResolve,
                ProductionCall::RuntimeBuild,
                ProductionCall::RunWithFrontend,
            ],
        );
        assert!(matches!(
            report.disposition,
            TestDisposition::Rejected | TestDisposition::Completed
        ));
        assert!(!report.mutated_source_bytes_reached_guest);
        if report.disposition == TestDisposition::Completed {
            assert_staged_snapshot_matches_fixture(&report);
            assert!(report.ownership.quiescence_proven);
        } else {
            assert!(report.ownership.controls || report.ownership.quiescence_proven);
        }
    }
}

#[test]
fn identity_mismatch_and_ambiguous_spawn_never_retry_or_adopt_by_name() {
    let mismatch = exercise(AppleCommandScenario::IdentityMismatch);
    assert_eq!(mismatch.disposition, TestDisposition::Rejected);
    assert_production_prefix(
        &mismatch,
        &[
            ProductionCall::AgentEngineBuildOptions,
            ProductionCall::ContainerOptionsResolve,
        ],
    );
    assert_no_launch_effects(&mismatch);

    let ambiguous = exercise(AppleCommandScenario::SpawnAmbiguous);
    assert_full_orchestrated_spawn_path(&ambiguous);
    assert_exact_owned_retention(&ambiguous);
    assert_eq!(ambiguous.spawn_attempts, 1);
    assert_eq!(ambiguous.child_starts, 1);
    assert_eq!(ambiguous.provider_entries_before_release, 0);
    assert_eq!(ambiguous.provider_entries_total, 0);
}

#[test]
fn cancellation_before_and_after_ready_preserves_or_proves_custody() {
    for point in [
        CancellationPoint::BeforeReady,
        CancellationPoint::AfterReadyBeforeRelease,
    ] {
        let report = exercise(AppleCommandScenario::Cancel(point));
        assert_full_orchestrated_spawn_path(&report);
        assert!(matches!(
            report.disposition,
            TestDisposition::Cancelled | TestDisposition::Retained
        ));
        assert_eq!(report.provider_entries_before_release, 0);
        assert_eq!(report.provider_entries_total, 0);
        assert_eq!(report.accepted_releases, 0);
        assert!(
            report.ownership.quiescence_proven
                || (report.ownership.registry_ticket
                    && report.ownership.controls
                    && report.ownership.staged_files
                    && report.ownership.child
                    && report.ownership.container),
            "cancellation at {point:?} lost custody"
        );
        assert!(!report.second_spawn_attempted);
        assert!(!report.name_only_cleanup_attempted);
    }
}

#[test]
fn uncertain_cleanup_stops_at_the_first_unproven_step_and_retains() {
    for (fault, expected_steps) in [
        (CleanupFault::TupleChanged, vec![CleanupStep::Inspect]),
        (
            CleanupFault::InspectionUnavailable,
            vec![CleanupStep::Inspect],
        ),
        (
            CleanupFault::StopFailed,
            vec![CleanupStep::Inspect, CleanupStep::Stop],
        ),
        (
            CleanupFault::ReapUncertain,
            vec![CleanupStep::Inspect, CleanupStep::Stop, CleanupStep::Reap],
        ),
        (
            CleanupFault::RemoveFailed,
            vec![
                CleanupStep::Inspect,
                CleanupStep::Stop,
                CleanupStep::Reap,
                CleanupStep::Inspect,
                CleanupStep::Remove,
            ],
        ),
        (
            CleanupFault::PreRemoveInspectionUnavailable,
            vec![
                CleanupStep::Inspect,
                CleanupStep::Stop,
                CleanupStep::Reap,
                CleanupStep::Inspect,
            ],
        ),
        (
            CleanupFault::FinalAbsenceUnproven,
            vec![
                CleanupStep::Inspect,
                CleanupStep::Stop,
                CleanupStep::Reap,
                CleanupStep::Inspect,
                CleanupStep::Remove,
                CleanupStep::ProveAbsent,
            ],
        ),
    ] {
        let report = exercise(AppleCommandScenario::CleanupUncertain(fault));
        assert_full_orchestrated_spawn_path(&report);
        assert_exact_owned_retention(&report);
        assert_eq!(report.provider_entries_before_release, 0);
        assert_eq!(report.provider_entries_total, 1);
        assert_eq!(report.accepted_releases, 1);
        assert_eq!(
            report.cleanup_steps, expected_steps,
            "cleanup fault {fault:?} crossed an uncertain step"
        );
    }
}

#[test]
fn legacy_gate_keeps_its_existing_mount_and_never_becomes_orchestrated() {
    let report = exercise(AppleCommandScenario::Legacy);

    assert_eq!(report.disposition, TestDisposition::Completed);
    assert_eq!(
        report.production_calls,
        vec![
            ProductionCall::AgentEngineResolve,
            ProductionCall::ContainerOptionsResolve,
            ProductionCall::RuntimeBuild,
            ProductionCall::RunWithFrontend,
        ]
    );
    assert_eq!(report.reserved_label_values, Vec::<String>::new());
    assert_eq!(report.expected_reserved_label, None);
    assert!(!report.legacy_orchestrated_plan);
    assert!(!report.registry_arc_reached_spawn);
    assert_eq!(
        report.writable_mounts,
        vec![MountObservation {
            container_path: CONTROL_PATH.into(),
            writable: true,
            source_revalidated: false,
        }]
    );
    assert_eq!(report.provider_entries_before_release, 0);
    assert_eq!(report.provider_entries_total, 1);
}

#[test]
fn external_release_is_required_before_simulated_provider_entry() {
    let report = exercise(AppleCommandScenario::ExternalRelease);

    assert_full_orchestrated_spawn_path(&report);
    assert_eq!(report.disposition, TestDisposition::Completed);
    assert!(report.protected_receipt_observed);
    assert!(report.release_absent_at_protected_receipt);
    assert_eq!(report.actual_provider_entries_before_release, 0);
    assert_eq!(report.released_acknowledgments, 1);
    assert_eq!(report.actual_provider_entries_total, 1);
    assert!(!report.second_spawn_attempted);
    assert!(!report.name_only_cleanup_attempted);
}

#[test]
fn drop_without_wait_transfers_one_loan_to_one_retryable_ticket() {
    let report = exercise(AppleCommandScenario::DropWithoutWait(
        RetentionRetryFault::None,
    ));

    assert_full_orchestrated_spawn_path(&report);
    assert!(report.protected_receipt_observed);
    assert!(report.release_absent_at_protected_receipt);
    assert_eq!(report.actual_provider_entries_before_release, 0);
    assert_eq!(report.actual_provider_entries_total, 0);
    assert!(
        report
            .drop_elapsed
            .is_some_and(|elapsed| elapsed <= Duration::from_secs(2)),
        "drop must transfer unresolved command custody within the existing two-second bound: {:?}",
        report.drop_elapsed
    );
    assert!(report.retained_ticket_before_retry);
    assert_eq!(report.active_resource_loans_before_retry, 1);
    assert_eq!(report.resource_loan_releases_before_retry, 0);
    assert!(!report.second_spawn_attempted);
    assert!(!report.name_only_cleanup_attempted);
}

#[test]
fn retained_ticket_retry_converges_only_after_exact_cleanup() {
    for (fault, expected_retry, retained, active_loans, releases) in [
        (
            RetentionRetryFault::None,
            RetryObservation::Resolved,
            false,
            0,
            1,
        ),
        (
            RetentionRetryFault::InspectionUnavailable,
            RetryObservation::Retained,
            true,
            1,
            0,
        ),
    ] {
        let report = exercise(AppleCommandScenario::DropWithoutWait(fault));

        assert_full_orchestrated_spawn_path(&report);
        assert_eq!(report.retry_observation, Some(expected_retry));
        assert_eq!(report.retained_ticket_after_retry, retained);
        assert_eq!(report.active_resource_loans_after_retry, active_loans);
        assert_eq!(report.resource_loan_releases_after_retry, releases);
        assert_eq!(report.actual_provider_entries_total, 0);
        assert!(!report.second_spawn_attempted);
        assert!(!report.name_only_cleanup_attempted);
    }
}

#[test]
fn protected_receipt_is_invisible_until_second_exact_inspection_succeeds() {
    for release in [
        SecondInspectionRelease::Unavailable,
        SecondInspectionRelease::Matching,
    ] {
        let report = exercise(AppleCommandScenario::ReceiptSecondInspection(release));

        assert_full_orchestrated_spawn_path(&report);
        assert_eq!(
            report.receipt_exists_while_second_inspection_blocked,
            Some(false),
            "protected receipt became visible before second inspection resolved for {release:?}"
        );
        assert!(report.release_absent_at_protected_receipt);
        assert_eq!(report.actual_provider_entries_before_release, 0);
        match release {
            SecondInspectionRelease::Unavailable => {
                assert!(matches!(
                    report.disposition,
                    TestDisposition::Rejected | TestDisposition::Retained
                ));
                assert!(!report.protected_receipt_observed);
                assert_eq!(report.released_acknowledgments, 0);
                assert_eq!(report.actual_provider_entries_total, 0);
                assert_eq!(
                    report.receipt_ever_observed_after_second_inspection,
                    Some(false)
                );
                assert_eq!(report.receipt_exists_after_execution, Some(false));
                assert_eq!(report.release_exists_after_execution, Some(false));
            }
            SecondInspectionRelease::Matching => {
                assert_eq!(report.disposition, TestDisposition::Completed);
                assert!(report.protected_receipt_observed);
                assert_eq!(report.released_acknowledgments, 1);
                assert_eq!(report.actual_provider_entries_total, 1);
                assert_eq!(
                    report.receipt_ever_observed_after_second_inspection,
                    Some(true)
                );
                assert_eq!(report.receipt_exists_after_execution, Some(true));
                assert_eq!(report.release_exists_after_execution, Some(true));
            }
        }
    }
}

#[test]
fn same_ticket_retries_after_first_inspection_uncertainty() {
    let report = exercise(AppleCommandScenario::RepeatRetry(
        RetryFaultPoint::CleanupInspection,
        RetryOrigin::Drop,
    ));

    assert_full_orchestrated_spawn_path(&report);
    assert_eq!(report.retry_tickets.len(), 2);
    assert_eq!(report.retry_tickets[0], report.retry_tickets[1]);
    assert_eq!(
        report.retry_observations,
        vec![RetryObservation::Retained, RetryObservation::Resolved]
    );
    assert_eq!(report.retained_after_retry_attempts, vec![true, false]);
    assert_eq!(
        report.active_resource_loans_after_retry_attempts,
        vec![1, 0]
    );
    assert_eq!(
        report.resource_loan_releases_after_retry_attempts,
        vec![0, 1]
    );
    assert_eq!(
        report.cleanup_steps_after_retry_attempts,
        vec![
            vec![CleanupStep::Inspect],
            vec![
                CleanupStep::Inspect,
                CleanupStep::Inspect,
                CleanupStep::Stop,
                CleanupStep::Reap,
                CleanupStep::Inspect,
                CleanupStep::Remove,
                CleanupStep::ProveAbsent,
            ],
        ]
    );
    assert_eq!(report.actual_provider_entries_total, 0);
    assert!(!report.second_spawn_attempted);
    assert!(!report.name_only_cleanup_attempted);
}

#[test]
fn same_ticket_retries_final_absence_without_repeating_remove() {
    for (origin, expected_provider_entries) in
        [(RetryOrigin::Drop, 0), (RetryOrigin::NormalWait, 1)]
    {
        let report = exercise(AppleCommandScenario::RepeatRetry(
            RetryFaultPoint::FinalAbsence,
            origin,
        ));

        assert_full_orchestrated_spawn_path(&report);
        assert_eq!(report.retry_tickets.len(), 2);
        assert_eq!(report.retry_tickets[0], report.retry_tickets[1]);
        assert_eq!(
            report.retry_observations,
            vec![RetryObservation::Retained, RetryObservation::Resolved]
        );
        assert_eq!(report.retained_after_retry_attempts, vec![true, false]);
        assert_eq!(
            report.active_resource_loans_after_retry_attempts,
            vec![1, 0]
        );
        assert_eq!(
            report.resource_loan_releases_after_retry_attempts,
            vec![0, 1]
        );
        assert_eq!(
            report.cleanup_steps_after_retry_attempts,
            vec![
                vec![
                    CleanupStep::Inspect,
                    CleanupStep::Stop,
                    CleanupStep::Reap,
                    CleanupStep::Inspect,
                    CleanupStep::Remove,
                    CleanupStep::ProveAbsent,
                ],
                vec![
                    CleanupStep::Inspect,
                    CleanupStep::Stop,
                    CleanupStep::Reap,
                    CleanupStep::Inspect,
                    CleanupStep::Remove,
                    CleanupStep::ProveAbsent,
                    CleanupStep::ProveAbsent,
                ],
            ],
            "retry from {origin:?} repeated a completed provider operation"
        );
        assert_eq!(
            report.actual_provider_entries_total,
            expected_provider_entries
        );
        assert!(!report.second_spawn_attempted);
        assert!(!report.name_only_cleanup_attempted);
    }
}

#[test]
fn same_ticket_retries_after_remove_effect_returns_error() {
    let report = exercise(AppleCommandScenario::RepeatRetry(
        RetryFaultPoint::RemoveEffectThenError,
        RetryOrigin::Drop,
    ));

    assert_full_orchestrated_spawn_path(&report);
    assert_eq!(report.retry_tickets.len(), 2);
    assert_eq!(report.retry_tickets[0], report.retry_tickets[1]);
    assert_eq!(
        report.retry_observations,
        vec![RetryObservation::Retained, RetryObservation::Resolved]
    );
    assert_eq!(report.retained_after_retry_attempts, vec![true, false]);
    assert_eq!(
        report.active_resource_loans_after_retry_attempts,
        vec![1, 0]
    );
    assert_eq!(
        report.resource_loan_releases_after_retry_attempts,
        vec![0, 1]
    );
    assert_eq!(
        report.cleanup_steps_after_retry_attempts,
        vec![
            vec![
                CleanupStep::Inspect,
                CleanupStep::Stop,
                CleanupStep::Reap,
                CleanupStep::Inspect,
                CleanupStep::Remove,
            ],
            vec![
                CleanupStep::Inspect,
                CleanupStep::Stop,
                CleanupStep::Reap,
                CleanupStep::Inspect,
                CleanupStep::Remove,
                CleanupStep::ProveAbsent,
            ],
        ]
    );
    assert_eq!(report.actual_provider_entries_total, 0);
    assert!(!report.second_spawn_attempted);
    assert!(!report.name_only_cleanup_attempted);
}

#[test]
fn production_supervisor_retries_the_same_active_ticket_without_repeating_effects() {
    let report = exercise(AppleCommandScenario::SupervisorRetry(
        RetryFaultPoint::FinalAbsence,
    ));

    assert_full_orchestrated_spawn_path(&report);
    assert_eq!(report.retry_tickets.len(), 2);
    assert_eq!(report.retry_tickets[0], report.retry_tickets[1]);
    assert_eq!(
        report.retry_observations,
        vec![RetryObservation::Retained, RetryObservation::Resolved]
    );
    assert_eq!(report.retained_after_retry_attempts, vec![true, false]);
    assert_eq!(
        report.active_resource_loans_after_retry_attempts,
        vec![1, 0]
    );
    assert_eq!(
        report.resource_loan_releases_after_retry_attempts,
        vec![0, 1]
    );
    assert_eq!(report.cleanup_steps_after_retry_attempts.len(), 2);
    let retained_steps = &report.cleanup_steps_after_retry_attempts[0];
    assert_eq!(
        retained_steps.get(..5),
        Some(
            [
                CleanupStep::Inspect,
                CleanupStep::Stop,
                CleanupStep::Reap,
                CleanupStep::Inspect,
                CleanupStep::Remove,
            ]
            .as_slice()
        )
    );
    assert_eq!(retained_steps.last(), Some(&CleanupStep::ProveAbsent));
    let final_steps = report
        .cleanup_steps_after_retry_attempts
        .last()
        .expect("supervisor retry must record its final cleanup phase");
    assert!(final_steps.len() > retained_steps.len());
    assert_eq!(final_steps.last(), Some(&CleanupStep::ProveAbsent));
    assert_eq!(
        final_steps
            .iter()
            .filter(|step| **step == CleanupStep::Stop)
            .count(),
        1
    );
    assert_eq!(
        final_steps
            .iter()
            .filter(|step| **step == CleanupStep::Remove)
            .count(),
        1
    );
    assert_eq!(report.actual_provider_entries_total, 0);
    assert!(!report.second_spawn_attempted);
    assert!(!report.name_only_cleanup_attempted);
}
