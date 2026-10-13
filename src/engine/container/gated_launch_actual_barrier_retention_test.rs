use std::error::Error;
use std::io::Write as _;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use sha2::{Digest, Sha256};

use super::gated_launch::child_lifecycle_test_support::NeverCreatedRetentionGate;
use super::gated_launch::retention_test_support::{registry_probe, RetentionProbe};
use super::gated_launch::{
    canonical_inspection_revision, prepare_orchestrated_launch, AbsenceObservation,
    CanonicalInspectionRevisionInput, CreateCliExitClass, CreateCliExitEvidence, DurableLaunchPlan,
    ExactInspection, GatedProviderAdapter, ImmutableImageId, InspectionObservationKind,
    LaunchIdentityError, LaunchRetentionReason, LaunchRetentionRegistry, LaunchRetentionTicket,
    NamePresence, ProviderCallDeadline, ProviderKind, ProviderLaunchInspection,
    SanitizedProviderStateObservation,
};
use super::options::{ContainerName, ImageRef};
use super::process::test_support::{
    spawn_actual_barrier_fixture_with_full_custody, ActualBarrierCustodyProbe, FixtureIoMode,
    FixtureSpawnGate, FixtureSpawnResult, FixtureSpawnSpec,
};
use crate::data::startup_gate::{load_startup_gate, StartupGateSpec};
use crate::engine::error::EngineError;

const IMAGE: &str = "sha256:abababababababababababababababababababababababababababababababab";
const EXECUTABLE: &str = "/usr/bin/touch";
const RETRY_BOUND: Duration = Duration::from_secs(4);
const CONFLICTING_CLEANUP_MARKER: &[u8] = b"test-owned actual-barrier conflict\n";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InspectionState {
    Absent = 0,
    Present = 1,
    Unavailable = 2,
}

impl InspectionState {
    fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Absent,
            1 => Self::Present,
            2 => Self::Unavailable,
            _ => panic!("invalid inspection state"),
        }
    }
}

struct BarrierAdapter {
    state: AtomicU8,
    absence_calls: AtomicUsize,
    launch_inspections: AtomicUsize,
    stop_calls: AtomicUsize,
    remove_calls: AtomicUsize,
}

impl Default for BarrierAdapter {
    fn default() -> Self {
        Self {
            state: AtomicU8::new(InspectionState::Absent as u8),
            absence_calls: AtomicUsize::new(0),
            launch_inspections: AtomicUsize::new(0),
            stop_calls: AtomicUsize::new(0),
            remove_calls: AtomicUsize::new(0),
        }
    }
}

impl BarrierAdapter {
    fn set_state(&self, state: InspectionState) {
        self.state.store(state as u8, Ordering::Release);
    }

    fn reset_after_plan(&self) {
        self.absence_calls.store(0, Ordering::Release);
        self.launch_inspections.store(0, Ordering::Release);
        self.stop_calls.store(0, Ordering::Release);
        self.remove_calls.store(0, Ordering::Release);
    }

    fn count(counter: &AtomicUsize) -> usize {
        counter.load(Ordering::Acquire)
    }

    fn no_created_runtime_actions(&self) -> bool {
        Self::count(&self.launch_inspections) == 0
            && Self::count(&self.stop_calls) == 0
            && Self::count(&self.remove_calls) == 0
    }
}

impl GatedProviderAdapter for BarrierAdapter {
    fn provider(&self) -> ProviderKind {
        ProviderKind::AppleContainers
    }

    fn resolve_image_identity(
        &self,
        _image: &ImageRef,
        _deadline: ProviderCallDeadline,
    ) -> Result<ImmutableImageId, LaunchIdentityError> {
        ImmutableImageId::new(IMAGE)
    }

    fn inspect_name_absence(
        &self,
        name: &ContainerName,
        _deadline: ProviderCallDeadline,
    ) -> Result<AbsenceObservation, NamePresence> {
        self.absence_calls.fetch_add(1, Ordering::AcqRel);
        match InspectionState::from_u8(self.state.load(Ordering::Acquire)) {
            InspectionState::Absent => Ok(AbsenceObservation {
                provider: ProviderKind::AppleContainers,
                exact_name: name.clone(),
                checked_at: Utc::now(),
                revision: canonical_inspection_revision(CanonicalInspectionRevisionInput {
                    kind: InspectionObservationKind::Absent,
                    provider: ProviderKind::AppleContainers,
                    exact_name: name,
                    runtime_id: None,
                    token_digest: None,
                    immutable_image_id: None,
                    created_at: None,
                    state: SanitizedProviderStateObservation::Absent,
                }),
            }),
            InspectionState::Present => Err(NamePresence::Present),
            InspectionState::Unavailable => Err(NamePresence::Unavailable),
        }
    }

    fn inspect_launch(
        &self,
        _key: &super::gated_launch::ProviderLaunchKey,
        _deadline: ProviderCallDeadline,
    ) -> ExactInspection {
        self.launch_inspections.fetch_add(1, Ordering::AcqRel);
        ExactInspection::Unavailable
    }

    fn classify_create_exit(&self, _exit: &CreateCliExitEvidence) -> CreateCliExitClass {
        CreateCliExitClass::Other
    }

    fn stop_inspected(
        &self,
        _inspection: &ProviderLaunchInspection,
        _deadline: ProviderCallDeadline,
    ) -> Result<(), EngineError> {
        self.stop_calls.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    fn remove_inspected(
        &self,
        _inspection: &ProviderLaunchInspection,
        _deadline: ProviderCallDeadline,
    ) -> Result<(), EngineError> {
        self.remove_calls.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
}

struct ControlFixture {
    _temp: tempfile::TempDir,
    parent: std::path::PathBuf,
    started_marker: std::path::PathBuf,
}

impl ControlFixture {
    fn new(name: &str, token: &str) -> Result<Self, Box<dyn Error>> {
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

        let temp = tempfile::tempdir()?;
        let parent = temp.path().join("control");
        let guest = parent.join("guest-control");
        let started_marker = temp.path().join("child-started.marker");
        std::fs::create_dir(&parent)?;
        std::fs::create_dir(&guest)?;
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700))?;
        std::fs::set_permissions(&guest, std::fs::Permissions::from_mode(0o700))?;

        let manifest = b"{\"version\":1,\"entries\":[]}\n";
        let manifest_digest = lower_hex(&Sha256::digest(manifest));
        let request = format!(
            "{{\"version\":1,\"bindings\":[{{\"id\":\"workspace\",\"workspace_path\":\"/workspace\",\"manifest_id\":\"{manifest_digest}\",\"manifest_file\":\"manifest.json\",\"access\":\"read-only\"}}]}}\n"
        );
        let intent = format!(
            "{{\"version\":1,\"container_name\":\"{name}\",\"launch_token\":\"{token}\"}}\n"
        );
        for (record, bytes) in [
            ("manifest.json", manifest.as_slice()),
            ("request.json", request.as_bytes()),
            ("launch-intent.json", intent.as_bytes()),
        ] {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(parent.join(record))?;
            file.write_all(bytes)?;
            file.sync_all()?;
        }
        Ok(Self {
            _temp: temp,
            parent,
            started_marker,
        })
    }

    fn plan(
        &self,
        adapter: &dyn GatedProviderAdapter,
    ) -> Result<(StartupGateSpec, DurableLaunchPlan), Box<dyn Error>> {
        let spec = load_startup_gate(&self.parent, Duration::from_secs(30))?;
        let (controls, identity) = spec
            .control
            .orchestrated_parts()
            .ok_or("expected validated orchestrated controls")?;
        let plan = prepare_orchestrated_launch(
            adapter,
            &ImageRef::new("awman-actual-barrier:latest"),
            identity,
            Arc::clone(controls),
            Instant::now() + RETRY_BOUND,
        )?;
        Ok((spec, plan))
    }

    fn cleanup_record(&self) -> Result<Vec<u8>, std::io::Error> {
        std::fs::read(self.parent.join("cleanup.json"))
    }

    fn cleanup_record_exists(&self) -> bool {
        self.parent.join("cleanup.json").exists()
    }

    fn child_started(&self) -> bool {
        self.started_marker.exists()
    }

    fn write_conflicting_cleanup_marker(&self) -> Result<(), std::io::Error> {
        use std::os::unix::fs::OpenOptionsExt as _;

        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(self.parent.join("cleanup.json"))?;
        file.write_all(CONFLICTING_CLEANUP_MARKER)?;
        file.sync_all()
    }

    fn remove_conflicting_cleanup_marker(&self) -> Result<(), std::io::Error> {
        std::fs::remove_file(self.parent.join("cleanup.json"))
    }

    fn launch_plan(&self) -> Result<Vec<u8>, std::io::Error> {
        std::fs::read(self.parent.join("launch-plan.json"))
    }
}

struct RefusedFixture {
    controls: ControlFixture,
    expected_plan: DurableLaunchPlan,
    adapter: Arc<BarrierAdapter>,
    spawn: FixtureSpawnResult,
    conversion_gate: NeverCreatedRetentionGate,
    custody: ActualBarrierCustodyProbe,
    ticket: LaunchRetentionTicket,
}

fn lower_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn fixture_spec(
    mode: FixtureIoMode,
    plan: DurableLaunchPlan,
    adapter: Arc<dyn GatedProviderAdapter>,
    started_marker: &std::path::Path,
) -> FixtureSpawnSpec {
    FixtureSpawnSpec {
        mode,
        executable: EXECUTABLE,
        args: vec![started_marker.display().to_string()],
        seeded_prompt: None,
        grace_timeout: Duration::from_millis(50),
        stuck_timeout: Duration::from_millis(50),
        gate: Some(FixtureSpawnGate {
            plan,
            adapter,
            enclosing: Instant::now() + RETRY_BOUND,
        }),
        fault: None,
    }
}

fn spawn_refused(
    registry: Arc<LaunchRetentionRegistry>,
    probe: &RetentionProbe,
    mode: FixtureIoMode,
    refusal: InspectionState,
    name: &str,
    token: &str,
) -> Result<RefusedFixture, Box<dyn Error>> {
    let controls = ControlFixture::new(name, token)?;
    let adapter = Arc::new(BarrierAdapter::default());
    let (startup_gate, plan) = controls.plan(adapter.as_ref())?;
    let expected_plan = plan.clone();
    adapter.reset_after_plan();
    adapter.set_state(refusal);
    let provider: Arc<dyn GatedProviderAdapter> = adapter.clone();
    let (spawn, conversion_gate, custody) = spawn_actual_barrier_fixture_with_full_custody(
        fixture_spec(mode, plan, provider, &controls.started_marker),
        registry,
        startup_gate,
    )?;
    let error = match &spawn.result {
        Ok(_) => return Err("actual barrier refusal unexpectedly started the CLI".into()),
        Err(error) => format!("{error:?}"),
    };
    let expected_error = match refusal {
        InspectionState::Present => "container name collision",
        InspectionState::Unavailable => "provider inspection unavailable",
        InspectionState::Absent => return Err("refused fixture requires a refusal".into()),
    };
    if !error.contains(expected_error) {
        return Err(format!("unexpected actual barrier error: {error}").into());
    }
    let ticket = probe
        .ticket_for_exact_plan(&expected_plan)
        .ok_or("retained exact-plan ticket was not uniquely observable")?;
    Ok(RefusedFixture {
        controls,
        expected_plan,
        adapter,
        spawn,
        conversion_gate,
        custody,
        ticket,
    })
}

fn assert_uncertain(probe: &RetentionProbe, fixture: &RefusedFixture) {
    let snapshot = probe
        .never_created_reconciliation_snapshot(fixture.ticket)
        .expect("actual-barrier reconciliation snapshot");
    assert_eq!(snapshot.ticket, fixture.ticket);
    assert!(snapshot.ticket_present);
    assert!(snapshot.entry_present);
    assert_eq!(snapshot.reason, LaunchRetentionReason::ChildStateUnknown);
    assert!(snapshot.reconciliation_pending);
    assert!(!snapshot.proof_present);
    assert_eq!(snapshot.resource_loans, 0);
    assert_eq!(snapshot.resource_loan_releases, 0);
    assert!(!snapshot.cleanup_complete);
    assert!(probe.retains_exact_plan(fixture.ticket, &fixture.expected_plan));
    assert_eq!(fixture.custody.aggregate_releases(), 0);
    assert_eq!(fixture.custody.active_leases(), 1);
    assert!(fixture.custody.gate_directory_exists());
    assert!(fixture.spawn.slot.authority().is_none());
    assert!(!fixture.controls.child_started());
    assert!(!fixture.controls.cleanup_record_exists());
    assert_eq!(BarrierAdapter::count(&fixture.adapter.absence_calls), 1);
    assert!(fixture.adapter.no_created_runtime_actions());
}

fn assert_reconciled(probe: &RetentionProbe, fixture: &RefusedFixture) {
    let snapshot = probe
        .never_created_reconciliation_snapshot(fixture.ticket)
        .expect("reconciled actual-barrier snapshot");
    assert_eq!(snapshot.ticket, fixture.ticket);
    assert_eq!(snapshot.reason, LaunchRetentionReason::SpawnResultUnknown);
    assert!(!snapshot.reconciliation_pending);
    assert!(snapshot.proof_present);
    assert_eq!(snapshot.resource_loans, 1);
    assert_eq!(snapshot.resource_loan_releases, 0);
    assert!(!snapshot.cleanup_complete);
    assert!(probe.retains_exact_plan(fixture.ticket, &fixture.expected_plan));
    assert_eq!(fixture.custody.aggregate_releases(), 0);
    assert_eq!(fixture.custody.active_leases(), 1);
    assert!(fixture.custody.gate_directory_exists());
    assert!(fixture.spawn.slot.authority().is_none());
    assert!(!fixture.controls.child_started());
    assert!(!fixture.controls.cleanup_record_exists());
    assert_eq!(BarrierAdapter::count(&fixture.adapter.absence_calls), 1);
    assert!(fixture.adapter.no_created_runtime_actions());
}

fn assert_resolved(probe: &RetentionProbe, fixture: &RefusedFixture) {
    assert!(!probe.contains(fixture.ticket));
    assert!(probe
        .never_created_reconciliation_snapshot(fixture.ticket)
        .is_none());
    assert_eq!(probe.resource_loan_releases(fixture.ticket), 1);
    assert_eq!(fixture.custody.aggregate_releases(), 1);
    assert_eq!(fixture.custody.active_leases(), 0);
    assert!(!fixture.custody.gate_directory_exists());
    assert!(fixture.spawn.slot.authority().is_none());
    assert!(!fixture.controls.child_started());
    assert_eq!(BarrierAdapter::count(&fixture.adapter.absence_calls), 2);
    assert!(fixture.adapter.no_created_runtime_actions());
}

fn assert_never_created_cleanup_record(fixture: &RefusedFixture) -> Result<(), Box<dyn Error>> {
    let bytes = fixture.controls.cleanup_record()?;
    let record: serde_json::Value = serde_json::from_slice(&bytes)?;
    let object = record
        .as_object()
        .ok_or("cleanup record was not a JSON object")?;
    let mut keys: Vec<_> = object.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, vec!["launchPlanSha256", "outcome", "version"]);
    assert_eq!(record["version"], 2);
    assert_eq!(record["outcome"], "absent");
    assert_eq!(
        record["launchPlanSha256"],
        lower_hex(&Sha256::digest(fixture.controls.launch_plan()?))
    );
    assert!(record.get("runtimeId").is_none());
    Ok(())
}

#[test]
fn present_actual_barrier_retains_full_custody_in_every_spawn_mode() -> Result<(), Box<dyn Error>> {
    for (index, (label, mode)) in [
        ("pty", FixtureIoMode::Pty { cols: 80, rows: 24 }),
        ("piped", FixtureIoMode::Piped),
        ("persistent", FixtureIoMode::PersistentPiped),
    ]
    .into_iter()
    .enumerate()
    {
        let registry = LaunchRetentionRegistry::try_new()?;
        let probe = registry_probe(&registry);
        let supervisor = probe.hold_supervisor_active_claims();
        let fixture = spawn_refused(
            Arc::clone(&registry),
            &probe,
            mode,
            InspectionState::Present,
            &format!("awman-altana-actual-barrier-{label}"),
            &format!("{:064x}", index + 1),
        )?;

        assert_eq!(probe.only_ticket(), Some(fixture.ticket));
        assert_uncertain(&probe, &fixture);
        assert_eq!(
            probe.advance_one_supervisor_active_attempt()?,
            Some(fixture.ticket)
        );
        assert_uncertain(&probe, &fixture);

        fixture.conversion_gate.release();
        assert_eq!(
            probe.advance_one_supervisor_active_attempt()?,
            Some(fixture.ticket)
        );
        assert_reconciled(&probe, &fixture);
        fixture.adapter.set_state(InspectionState::Absent);
        assert_eq!(
            probe.advance_one_supervisor_active_attempt()?,
            Some(fixture.ticket)
        );
        assert_resolved(&probe, &fixture);
        assert_never_created_cleanup_record(&fixture)?;

        drop(supervisor);
        drop(registry);
    }
    Ok(())
}

#[test]
fn unavailable_actual_barrier_retries_conversion_and_exact_cleanup_publication(
) -> Result<(), Box<dyn Error>> {
    let registry = LaunchRetentionRegistry::try_new()?;
    let probe = registry_probe(&registry);
    let supervisor = probe.hold_supervisor_active_claims();
    let fixture = spawn_refused(
        Arc::clone(&registry),
        &probe,
        FixtureIoMode::Piped,
        InspectionState::Unavailable,
        "awman-altana-actual-barrier-unavailable",
        "7171717171717171717171717171717171717171717171717171717171717171",
    )?;

    assert_eq!(probe.only_ticket(), Some(fixture.ticket));
    assert_uncertain(&probe, &fixture);
    assert_eq!(
        probe.advance_one_supervisor_active_attempt()?,
        Some(fixture.ticket)
    );
    assert_uncertain(&probe, &fixture);

    fixture.conversion_gate.release();
    assert_eq!(
        probe.advance_one_supervisor_active_attempt()?,
        Some(fixture.ticket)
    );
    assert_reconciled(&probe, &fixture);

    fixture.controls.write_conflicting_cleanup_marker()?;
    fixture.adapter.set_state(InspectionState::Absent);
    assert_eq!(
        probe.advance_one_supervisor_active_attempt()?,
        Some(fixture.ticket)
    );
    let retained = probe
        .never_created_reconciliation_snapshot(fixture.ticket)
        .ok_or("publication conflict must retain the same ticket")?;
    assert_eq!(
        retained.reason,
        LaunchRetentionReason::PostSpawnControlChanged
    );
    assert!(retained.proof_present);
    assert_eq!(retained.resource_loans, 1);
    assert_eq!(retained.resource_loan_releases, 0);
    assert!(probe.retains_exact_plan(fixture.ticket, &fixture.expected_plan));
    assert_eq!(fixture.custody.aggregate_releases(), 0);
    assert_eq!(fixture.custody.active_leases(), 1);
    assert!(fixture.custody.gate_directory_exists());
    assert_eq!(
        fixture.controls.cleanup_record()?,
        CONFLICTING_CLEANUP_MARKER
    );
    assert_eq!(BarrierAdapter::count(&fixture.adapter.absence_calls), 2);
    assert!(fixture.adapter.no_created_runtime_actions());
    assert!(!fixture.controls.child_started());

    fixture.controls.remove_conflicting_cleanup_marker()?;
    assert_eq!(
        probe.advance_one_supervisor_active_attempt()?,
        Some(fixture.ticket)
    );
    assert_resolved(&probe, &fixture);
    assert_never_created_cleanup_record(&fixture)?;

    drop(supervisor);
    drop(registry);
    Ok(())
}
