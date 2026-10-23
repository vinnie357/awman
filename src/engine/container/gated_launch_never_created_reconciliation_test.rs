use std::error::Error;
use std::io::Write as _;
use std::sync::atomic::{AtomicUsize, Ordering};
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
    spawn_fixture_with_never_created_retention_blocked, CustodyWitnessProbe, FixtureIoMode,
    FixtureSpawnGate, FixtureSpawnResult, FixtureSpawnSpec,
};
use crate::data::startup_gate::load_startup_gate;
use crate::engine::error::EngineError;

const IMAGE: &str = "sha256:abababababababababababababababababababababababababababababababab";
const MISSING_EXECUTABLE: &str = "/dev/null/awman-rc4-never-created";
const RETRY_BOUND: Duration = Duration::from_secs(4);
const CONFLICTING_CLEANUP_MARKER: &[u8] = b"test-owned publication conflict\n";

#[derive(Default)]
struct CountingAdapter {
    resolve_calls: AtomicUsize,
    absence_calls: AtomicUsize,
    launch_inspections: AtomicUsize,
    stop_calls: AtomicUsize,
    remove_calls: AtomicUsize,
}

impl CountingAdapter {
    fn reset_after_plan(&self) {
        self.resolve_calls.store(0, Ordering::Release);
        self.absence_calls.store(0, Ordering::Release);
        self.launch_inspections.store(0, Ordering::Release);
        self.stop_calls.store(0, Ordering::Release);
        self.remove_calls.store(0, Ordering::Release);
    }

    fn load(counter: &AtomicUsize) -> usize {
        counter.load(Ordering::Acquire)
    }

    fn no_created_runtime_cleanup_calls(&self) -> bool {
        Self::load(&self.launch_inspections) == 0
            && Self::load(&self.stop_calls) == 0
            && Self::load(&self.remove_calls) == 0
    }
}

impl GatedProviderAdapter for CountingAdapter {
    fn provider(&self) -> ProviderKind {
        ProviderKind::AppleContainers
    }

    fn resolve_image_identity(
        &self,
        _image: &ImageRef,
        _deadline: ProviderCallDeadline,
    ) -> Result<ImmutableImageId, LaunchIdentityError> {
        self.resolve_calls.fetch_add(1, Ordering::AcqRel);
        ImmutableImageId::new(IMAGE)
    }

    fn inspect_name_absence(
        &self,
        name: &ContainerName,
        _deadline: ProviderCallDeadline,
    ) -> Result<AbsenceObservation, NamePresence> {
        self.absence_calls.fetch_add(1, Ordering::AcqRel);
        Ok(AbsenceObservation {
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
        })
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
}

impl ControlFixture {
    fn new(name: &str, token: &str) -> Result<Self, Box<dyn Error>> {
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

        let temp = tempfile::tempdir()?;
        let parent = temp.path().join("control");
        let guest = parent.join("guest-control");
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
        })
    }

    fn plan(
        &self,
        adapter: &dyn GatedProviderAdapter,
    ) -> Result<DurableLaunchPlan, Box<dyn Error>> {
        let spec = load_startup_gate(&self.parent, Duration::from_secs(30))?;
        let (controls, identity) = spec
            .control
            .orchestrated_parts()
            .ok_or("expected validated orchestrated controls")?;
        Ok(prepare_orchestrated_launch(
            adapter,
            &ImageRef::new("awman-never-created-reconciliation:latest"),
            identity,
            Arc::clone(controls),
            Instant::now() + RETRY_BOUND,
        )?)
    }

    fn cleanup_record(&self) -> Result<Vec<u8>, std::io::Error> {
        std::fs::read(self.parent.join("cleanup.json"))
    }

    fn cleanup_record_exists(&self) -> bool {
        self.parent.join("cleanup.json").exists()
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

struct UncertainFixture {
    controls: ControlFixture,
    expected_plan: DurableLaunchPlan,
    adapter: Arc<CountingAdapter>,
    spawn: FixtureSpawnResult,
    gate: NeverCreatedRetentionGate,
    witness: CustodyWitnessProbe,
    ticket: LaunchRetentionTicket,
}

fn lower_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn fixture_spec(
    plan: DurableLaunchPlan,
    adapter: Arc<dyn GatedProviderAdapter>,
) -> FixtureSpawnSpec {
    FixtureSpawnSpec {
        mode: FixtureIoMode::Piped,
        executable: MISSING_EXECUTABLE,
        args: Vec::new(),
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

fn spawn_uncertain(
    registry: Arc<LaunchRetentionRegistry>,
    probe: &RetentionProbe,
    name: &str,
    token: &str,
) -> Result<UncertainFixture, Box<dyn Error>> {
    let controls = ControlFixture::new(name, token)?;
    let adapter = Arc::new(CountingAdapter::default());
    let plan = controls.plan(adapter.as_ref())?;
    let expected_plan = plan.clone();
    adapter.reset_after_plan();
    let provider: Arc<dyn GatedProviderAdapter> = adapter.clone();
    let (spawn, gate, witness) = spawn_fixture_with_never_created_retention_blocked(
        fixture_spec(plan, provider),
        Arc::clone(&registry),
    )?;
    if spawn.result.is_ok() {
        return Err("guaranteed-missing executable unexpectedly started".into());
    }
    let ticket = probe
        .ticket_for_exact_plan(&expected_plan)
        .ok_or("retained exact-plan ticket was not uniquely observable")?;
    Ok(UncertainFixture {
        controls,
        expected_plan,
        adapter,
        spawn,
        gate,
        witness,
        ticket,
    })
}

fn assert_blocked(probe: &RetentionProbe, fixture: &UncertainFixture) {
    let snapshot = probe
        .never_created_reconciliation_snapshot(fixture.ticket)
        .expect("blocked reconciliation snapshot");
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
    assert_eq!(fixture.witness.releases(), 0);
    assert!(fixture.spawn.slot.authority().is_none());
    assert!(!fixture.controls.cleanup_record_exists());
    assert_eq!(CountingAdapter::load(&fixture.adapter.absence_calls), 1);
    assert!(fixture.adapter.no_created_runtime_cleanup_calls());
}

fn assert_reconciled_before_cleanup(probe: &RetentionProbe, fixture: &UncertainFixture) {
    let snapshot = probe
        .never_created_reconciliation_snapshot(fixture.ticket)
        .expect("reconciled snapshot");
    assert_eq!(snapshot.ticket, fixture.ticket);
    assert!(snapshot.ticket_present);
    assert!(snapshot.entry_present);
    assert_eq!(snapshot.reason, LaunchRetentionReason::SpawnResultUnknown);
    assert!(!snapshot.reconciliation_pending);
    assert!(snapshot.proof_present);
    assert_eq!(snapshot.resource_loans, 1);
    assert_eq!(snapshot.resource_loan_releases, 0);
    assert!(!snapshot.cleanup_complete);
    assert!(probe.retains_exact_plan(fixture.ticket, &fixture.expected_plan));
    assert_eq!(fixture.witness.releases(), 0);
    assert!(fixture.spawn.slot.authority().is_none());
    assert!(!fixture.controls.cleanup_record_exists());
    assert_eq!(CountingAdapter::load(&fixture.adapter.absence_calls), 1);
    assert!(fixture.adapter.no_created_runtime_cleanup_calls());
}

fn assert_publication_conflict_retained(
    probe: &RetentionProbe,
    fixture: &UncertainFixture,
) -> Result<(), Box<dyn Error>> {
    let snapshot = probe
        .never_created_reconciliation_snapshot(fixture.ticket)
        .expect("publication-conflict snapshot");
    assert_eq!(snapshot.ticket, fixture.ticket);
    assert!(snapshot.ticket_present);
    assert!(snapshot.entry_present);
    assert_eq!(
        snapshot.reason,
        LaunchRetentionReason::PostSpawnControlChanged
    );
    assert!(!snapshot.reconciliation_pending);
    assert!(snapshot.proof_present);
    assert_eq!(snapshot.resource_loans, 1);
    assert_eq!(snapshot.resource_loan_releases, 0);
    assert!(!snapshot.cleanup_complete);
    assert!(probe.retains_exact_plan(fixture.ticket, &fixture.expected_plan));
    assert_eq!(fixture.witness.releases(), 0);
    assert!(fixture.spawn.slot.authority().is_none());
    assert_eq!(
        fixture.controls.cleanup_record()?,
        CONFLICTING_CLEANUP_MARKER
    );
    assert_eq!(CountingAdapter::load(&fixture.adapter.absence_calls), 2);
    assert!(fixture.adapter.no_created_runtime_cleanup_calls());
    Ok(())
}

fn assert_never_created_cleanup_record(fixture: &UncertainFixture) -> Result<(), Box<dyn Error>> {
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

fn assert_resolved(probe: &RetentionProbe, fixture: &UncertainFixture) {
    assert!(!probe.contains(fixture.ticket));
    assert!(probe
        .never_created_reconciliation_snapshot(fixture.ticket)
        .is_none());
    assert_eq!(probe.resource_loan_releases(fixture.ticket), 1);
    assert_eq!(fixture.witness.releases(), 1);
    assert!(fixture.spawn.slot.authority().is_none());
    assert_eq!(CountingAdapter::load(&fixture.adapter.absence_calls), 2);
    assert!(fixture.adapter.no_created_runtime_cleanup_calls());
}

#[test]
fn never_created_retention_reconciles_same_ticket_before_cleanup() -> Result<(), Box<dyn Error>> {
    let registry = LaunchRetentionRegistry::try_new()?;
    let probe = registry_probe(&registry);
    let supervisor = probe.hold_supervisor_active_claims();
    let fixture = spawn_uncertain(
        Arc::clone(&registry),
        &probe,
        "awman-altana-never-created-reconcile",
        "4141414141414141414141414141414141414141414141414141414141414141",
    )?;

    assert_eq!(probe.only_ticket(), Some(fixture.ticket));
    assert_blocked(&probe, &fixture);

    let blocked_retry = probe.advance_one_supervisor_active_attempt()?;
    assert_eq!(blocked_retry, Some(fixture.ticket));
    assert_eq!(probe.only_ticket(), Some(fixture.ticket));
    assert_blocked(&probe, &fixture);

    fixture.gate.release();
    let reconciliation = probe.advance_one_supervisor_active_attempt()?;
    assert_eq!(reconciliation, Some(fixture.ticket));
    assert_eq!(probe.only_ticket(), Some(fixture.ticket));
    assert_reconciled_before_cleanup(&probe, &fixture);

    fixture.controls.write_conflicting_cleanup_marker()?;
    let rejected_publication = probe.advance_one_supervisor_active_attempt()?;
    assert_eq!(rejected_publication, Some(fixture.ticket));
    assert_eq!(probe.only_ticket(), Some(fixture.ticket));
    assert_publication_conflict_retained(&probe, &fixture)?;

    fixture.controls.remove_conflicting_cleanup_marker()?;
    let cleanup = probe.advance_one_supervisor_active_attempt()?;
    assert_eq!(cleanup, Some(fixture.ticket));
    assert_resolved(&probe, &fixture);
    assert_never_created_cleanup_record(&fixture)?;

    drop(supervisor);
    drop(registry);
    Ok(())
}

#[test]
fn never_created_retention_failure_is_one_attempt_per_ticket_and_round_robin_fair(
) -> Result<(), Box<dyn Error>> {
    let registry = LaunchRetentionRegistry::try_new()?;
    let probe = registry_probe(&registry);
    let supervisor = probe.hold_supervisor_active_claims();
    let first = spawn_uncertain(
        Arc::clone(&registry),
        &probe,
        "awman-altana-never-created-fairness-first",
        "5151515151515151515151515151515151515151515151515151515151515151",
    )?;
    let second = spawn_uncertain(
        Arc::clone(&registry),
        &probe,
        "awman-altana-never-created-fairness-second",
        "6161616161616161616161616161616161616161616161616161616161616161",
    )?;

    assert_ne!(first.ticket, second.ticket);
    assert_eq!(
        probe.ticket_for_exact_plan(&first.expected_plan),
        Some(first.ticket)
    );
    assert_eq!(
        probe.ticket_for_exact_plan(&second.expected_plan),
        Some(second.ticket)
    );
    assert_blocked(&probe, &first);
    assert_blocked(&probe, &second);

    let first_attempt = probe.advance_one_supervisor_active_attempt()?;
    assert_eq!(first_attempt, Some(first.ticket));
    assert_blocked(&probe, &first);
    assert_blocked(&probe, &second);

    let second_attempt = probe.advance_one_supervisor_active_attempt()?;
    assert_eq!(second_attempt, Some(second.ticket));
    assert_blocked(&probe, &first);
    assert_blocked(&probe, &second);

    let wrapped_attempt = probe.advance_one_supervisor_active_attempt()?;
    assert_eq!(wrapped_attempt, Some(first.ticket));
    assert_blocked(&probe, &first);
    assert_blocked(&probe, &second);

    first.gate.release();
    second.gate.release();
    assert_eq!(
        probe.advance_one_supervisor_active_attempt()?,
        Some(second.ticket)
    );
    assert_blocked(&probe, &first);
    assert_reconciled_before_cleanup(&probe, &second);
    assert_eq!(
        probe.advance_one_supervisor_active_attempt()?,
        Some(first.ticket)
    );
    assert_reconciled_before_cleanup(&probe, &first);
    assert_reconciled_before_cleanup(&probe, &second);
    assert_eq!(
        probe.advance_one_supervisor_active_attempt()?,
        Some(second.ticket)
    );
    assert_reconciled_before_cleanup(&probe, &first);
    assert_resolved(&probe, &second);
    assert_never_created_cleanup_record(&second)?;
    assert_eq!(
        probe.advance_one_supervisor_active_attempt()?,
        Some(first.ticket)
    );
    assert_resolved(&probe, &first);
    assert_never_created_cleanup_record(&first)?;

    drop(supervisor);
    drop(registry);
    Ok(())
}

#[test]
fn retained_retry_diagnostic_matches_blocked_never_created_reconciliation(
) -> Result<(), Box<dyn Error>> {
    use super::gated_launch::retention_test_support::retry_active_gated_with_diagnostic;
    use super::gated_launch::ActiveGatedRetryOutcome;

    struct ReleaseGateOnExit<'a>(&'a NeverCreatedRetentionGate);
    impl Drop for ReleaseGateOnExit<'_> {
        fn drop(&mut self) {
            self.0.release();
        }
    }

    let registry = LaunchRetentionRegistry::try_new()?;
    let probe = registry_probe(&registry);
    let supervisor = probe.hold_supervisor_active_claims();
    let fixture = spawn_uncertain(
        Arc::clone(&registry),
        &probe,
        "awman-altana-retry-diagnostic-reconciliation",
        "7171717171717171717171717171717171717171717171717171717171717171",
    )?;
    let release_gate = ReleaseGateOnExit(&fixture.gate);

    assert_eq!(probe.only_ticket(), Some(fixture.ticket));
    assert_blocked(&probe, &fixture);
    let (outcome, diagnostic) = retry_active_gated_with_diagnostic(
        &registry,
        fixture.ticket,
        Instant::now() + Duration::from_secs(2),
    )?;
    assert_eq!(outcome, ActiveGatedRetryOutcome::Retained);
    let diagnostic = diagnostic.ok_or("retained retry omitted its actual branch diagnostic")?;
    assert_eq!(diagnostic.branch, "ReconciliationPending");
    let custody = diagnostic
        .custody
        .ok_or("reconciliation branch omitted its owned active custody")?;
    assert_eq!(custody.reason, LaunchRetentionReason::ChildStateUnknown);
    assert_eq!(custody.cleanup_phase, "NeedsCleanup");
    assert!(!custody.resource_loan_present);
    assert!(!custody.gate_monitor_present);
    assert_eq!(probe.only_ticket(), Some(fixture.ticket));
    assert_blocked(&probe, &fixture);

    fixture.gate.release();
    let reconciliation = probe.advance_one_supervisor_active_attempt()?;
    assert_eq!(reconciliation, Some(fixture.ticket));
    assert_eq!(probe.only_ticket(), Some(fixture.ticket));
    assert_reconciled_before_cleanup(&probe, &fixture);
    let cleanup = probe.advance_one_supervisor_active_attempt()?;
    assert_eq!(cleanup, Some(fixture.ticket));
    assert_resolved(&probe, &fixture);
    assert_never_created_cleanup_record(&fixture)?;

    drop(release_gate);
    drop(supervisor);
    drop(registry);
    Ok(())
}
