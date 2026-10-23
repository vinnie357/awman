use std::error::Error;
use std::io::Write as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use sha2::{Digest, Sha256};

use super::gated_launch::retention_test_support::{
    active_plan_matches_publication, pending_cleanup_record_bytes,
    pending_publication_file_identity, pending_publication_snapshot, registry_probe,
    ticket_is_pending, PendingPublicationSnapshot, RetentionProbe,
};
use super::gated_launch::{
    canonical_inspection_revision, AbsenceObservation, CanonicalInspectionRevisionInput,
    CreateCliExitClass, CreateCliExitEvidence, ExactInspection, GatedProviderAdapter,
    ImmutableImageId, InspectionObservationKind, LaunchIdentityError, LaunchRetentionReason,
    LaunchRetentionRegistry, NamePresence, ProviderCallDeadline, ProviderKind,
    ProviderLaunchInspection, SanitizedProviderStateObservation,
};
use super::options::{ContainerName, ImageRef};
use super::process::test_support::{run_publication_custody_fixture, PublicationCustodyProbe};
use crate::data::startup_gate::{
    after_launch_plan_native_publication, arm_launch_plan_post_rename_sync_failure,
    load_startup_gate, OrchestratedControlAuthority, StartupGateSpec,
};
use crate::engine::error::EngineError;

const IMAGE: &str = "sha256:abababababababababababababababababababababababababababababababab";
const NAME: &str = "awman-altana-publication-acceptance";
const FOREIGN_PLAN: &[u8] = b"test-owned foreign launch plan\n";
const FOREIGN_CLEANUP: &[u8] = b"test-owned conflicting cleanup\n";

struct PublicationAdapter {
    cleanup_path: PathBuf,
    absence_calls: AtomicUsize,
    launch_inspections: AtomicUsize,
    stop_calls: AtomicUsize,
    remove_calls: AtomicUsize,
    cleanup_seen_during_absence: AtomicBool,
    present: AtomicBool,
    collide_cleanup_on_second_absence: AtomicBool,
}

impl PublicationAdapter {
    fn new(cleanup_path: PathBuf) -> Self {
        Self {
            cleanup_path,
            absence_calls: AtomicUsize::new(0),
            launch_inspections: AtomicUsize::new(0),
            stop_calls: AtomicUsize::new(0),
            remove_calls: AtomicUsize::new(0),
            cleanup_seen_during_absence: AtomicBool::new(false),
            present: AtomicBool::new(false),
            collide_cleanup_on_second_absence: AtomicBool::new(false),
        }
    }

    fn count(counter: &AtomicUsize) -> usize {
        counter.load(Ordering::Acquire)
    }
}

impl GatedProviderAdapter for PublicationAdapter {
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
        let call = self.absence_calls.fetch_add(1, Ordering::AcqRel) + 1;
        if self.present.load(Ordering::Acquire) {
            return Err(NamePresence::Present);
        }
        if self.cleanup_path.exists() {
            self.cleanup_seen_during_absence
                .store(true, Ordering::Release);
        }
        if call == 2
            && self
                .collide_cleanup_on_second_absence
                .load(Ordering::Acquire)
        {
            assert!(
                !self.cleanup_path.exists(),
                "cleanup must not precede exact absence"
            );
            write_private(&self.cleanup_path, FOREIGN_CLEANUP)
                .expect("create only the test-owned cleanup collision");
        }
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
    parent: PathBuf,
    cli_started: PathBuf,
    executable: &'static str,
}

impl ControlFixture {
    fn new() -> Result<Self, Box<dyn Error>> {
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
            "{{\"version\":1,\"container_name\":\"{NAME}\",\"launch_token\":\"{}\"}}\n",
            "11".repeat(32)
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

        let executable_path = temp.path().join("record-cli-start.sh");
        let mut executable_file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o700)
            .open(&executable_path)?;
        executable_file.write_all(b"#!/bin/sh\n: > \"${0}.started\"\nexit 0\n")?;
        executable_file.sync_all()?;
        let cli_started = PathBuf::from(format!("{}.started", executable_path.display()));
        let executable = Box::leak(
            executable_path
                .to_string_lossy()
                .into_owned()
                .into_boxed_str(),
        );
        Ok(Self {
            _temp: temp,
            parent,
            cli_started,
            executable,
        })
    }

    fn launch_plan_path(&self) -> PathBuf {
        self.parent.join("launch-plan.json")
    }

    fn cleanup_path(&self) -> PathBuf {
        self.parent.join("cleanup.json")
    }

    fn assert_cli_never_started(&self) {
        assert!(!self.cli_started.exists());
    }
}

fn lower_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn assert_no_runtime_actions(adapter: &PublicationAdapter) {
    assert_eq!(PublicationAdapter::count(&adapter.launch_inspections), 0);
    assert_eq!(PublicationAdapter::count(&adapter.stop_calls), 0);
    assert_eq!(PublicationAdapter::count(&adapter.remove_calls), 0);
}

fn write_private(path: &std::path::Path, bytes: &[u8]) -> Result<(), std::io::Error> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn file_identity(path: &std::path::Path) -> Result<(u64, u64, u64), std::io::Error> {
    let metadata = std::fs::symlink_metadata(path)?;
    Ok((metadata.dev(), metadata.ino(), metadata.nlink()))
}

fn load_controls(
    fixture: &ControlFixture,
) -> Result<(StartupGateSpec, Arc<OrchestratedControlAuthority>), Box<dyn Error>> {
    let spec = load_startup_gate(&fixture.parent, Duration::from_secs(30))?;
    let controls = Arc::clone(
        spec.control
            .orchestrated_parts()
            .ok_or("expected orchestrated controls")?
            .0,
    );
    Ok((spec, controls))
}

fn assert_run_error<T>(run: &Result<T, EngineError>, expected: &str) {
    match run {
        Err(EngineError::Container(message)) => assert_eq!(message, expected),
        Err(other) => panic!("unexpected publication error: {other:?}"),
        Ok(_) => panic!("publication rejection unexpectedly returned success"),
    }
}

fn assert_full_custody_held(
    probe: &RetentionProbe,
    snapshot: &PendingPublicationSnapshot,
    custody: &PublicationCustodyProbe,
) {
    assert_eq!(probe.only_ticket(), Some(snapshot.ticket));
    assert!(snapshot.same_controls);
    assert!(snapshot.prepared_present);
    assert!(snapshot.resource_bundle_present);
    assert_eq!(probe.resource_loan_releases(snapshot.ticket), 0);
    assert_eq!(custody.aggregate_releases(), 0);
    assert_eq!(custody.active_leases(), 1);
    assert!(custody.gate_directory_exists());
}

fn assert_pending_unchanged(
    fixture: &ControlFixture,
    registry: &LaunchRetentionRegistry,
    controls: &Arc<OrchestratedControlAuthority>,
    snapshot: &PendingPublicationSnapshot,
    custody: &PublicationCustodyProbe,
    adapter: &PublicationAdapter,
) {
    let probe = registry_probe(registry);
    assert!(ticket_is_pending(registry, snapshot.ticket));
    assert_eq!(
        pending_publication_snapshot(registry, controls),
        Some(snapshot.clone())
    );
    assert!(probe
        .never_created_reconciliation_snapshot(snapshot.ticket)
        .is_none());
    assert_full_custody_held(&probe, snapshot, custody);
    fixture.assert_cli_never_started();
    assert!(!fixture.cleanup_path().exists());
    assert_eq!(PublicationAdapter::count(&adapter.absence_calls), 1);
    assert_no_runtime_actions(adapter);
}

#[derive(Clone)]
struct PublicationObservation {
    identity: (u64, u64, u64),
    canonical_bytes: Vec<u8>,
}

#[derive(Clone, Copy)]
enum PublicationMutation {
    ReplaceGuestControl,
    ReplacePlanWithEqualBytes,
    MutateOriginalPlanBytes,
}

fn install_post_native_mutation(
    fixture: &ControlFixture,
    controls: &Arc<OrchestratedControlAuthority>,
    mutation: PublicationMutation,
) -> Arc<Mutex<Option<PublicationObservation>>> {
    let parent = fixture.parent.clone();
    let observation = Arc::new(Mutex::new(None));
    let captured = Arc::clone(&observation);
    after_launch_plan_native_publication(
        controls,
        Box::new(move || {
            let plan = parent.join("launch-plan.json");
            let canonical_bytes = std::fs::read(&plan).expect("read published original plan");
            let identity = file_identity(&plan).expect("observe original published identity");
            *captured.lock().expect("publication observation") = Some(PublicationObservation {
                identity,
                canonical_bytes: canonical_bytes.clone(),
            });
            match mutation {
                PublicationMutation::ReplaceGuestControl => {
                    let guest = parent.join("guest-control");
                    std::fs::rename(&guest, parent.join("original-guest-control"))
                        .expect("move only the original test-owned guest control");
                    std::fs::create_dir(&guest)
                        .expect("create test-owned replacement guest control");
                    std::fs::set_permissions(&guest, std::fs::Permissions::from_mode(0o700))
                        .expect("protect replacement guest control");
                }
                PublicationMutation::ReplacePlanWithEqualBytes => {
                    std::fs::remove_file(&plan).expect("unlink only the test-owned original plan");
                    write_private(&plan, &canonical_bytes).expect("create test-owned foreign plan");
                }
                PublicationMutation::MutateOriginalPlanBytes => {
                    let mut file = std::fs::OpenOptions::new()
                        .write(true)
                        .truncate(true)
                        .open(&plan)
                        .expect("open the original test-owned inode");
                    file.write_all(FOREIGN_PLAN)
                        .expect("mutate original inode bytes");
                    file.sync_all().expect("sync the test mutation");
                }
            }
        }),
    );
    observation
}

fn captured_publication(
    observation: &Arc<Mutex<Option<PublicationObservation>>>,
) -> Result<PublicationObservation, Box<dyn Error>> {
    observation
        .lock()
        .map_err(|_| "publication observation was poisoned")?
        .clone()
        .ok_or_else(|| "native success callback did not run".into())
}

fn initial_pending(
    registry: &LaunchRetentionRegistry,
    controls: &Arc<OrchestratedControlAuthority>,
    observed: &PublicationObservation,
) -> Result<PendingPublicationSnapshot, Box<dyn Error>> {
    let initial =
        pending_publication_snapshot(registry, controls).ok_or("one pending publication entry")?;
    assert_eq!(initial.canonical_bytes, observed.canonical_bytes);
    assert_eq!(
        initial.canonical_digest,
        <[u8; 32]>::from(Sha256::digest(&observed.canonical_bytes))
    );
    Ok(initial)
}

#[test]
fn post_native_control_replacement_retains_original_file_and_real_run_custody(
) -> Result<(), Box<dyn Error>> {
    let fixture = ControlFixture::new()?;
    let (spec, controls) = load_controls(&fixture)?;
    let original_guest = file_identity(&fixture.parent.join("guest-control"))?;
    let observation = install_post_native_mutation(
        &fixture,
        &controls,
        PublicationMutation::ReplaceGuestControl,
    );
    let adapter = Arc::new(PublicationAdapter::new(fixture.cleanup_path()));
    let provider: Arc<dyn GatedProviderAdapter> = adapter.clone();
    let registry = LaunchRetentionRegistry::try_new()?;
    let probe = registry_probe(&registry);
    let _supervisor = probe.hold_supervisor_active_claims();
    let (run, custody) =
        run_publication_custody_fixture(spec, provider, Arc::clone(&registry), fixture.executable)?;
    assert_run_error(&run, "launch plan publication failed");
    let observed = captured_publication(&observation)?;
    let initial = initial_pending(&registry, &controls, &observed)?;
    assert_eq!(
        file_identity(&fixture.parent.join("original-guest-control"))?,
        original_guest
    );
    let replacement_guest = file_identity(&fixture.parent.join("guest-control"))?;
    assert_ne!(replacement_guest, original_guest);
    assert_eq!(
        pending_publication_file_identity(&registry, initial.ticket),
        Some(observed.identity)
    );
    assert_pending_unchanged(&fixture, &registry, &controls, &initial, &custody, &adapter);
    assert_eq!(
        probe.advance_one_supervisor_active_attempt()?,
        Some(initial.ticket)
    );
    assert_pending_unchanged(&fixture, &registry, &controls, &initial, &custody, &adapter);
    assert_eq!(
        pending_publication_file_identity(&registry, initial.ticket),
        Some(observed.identity)
    );
    assert_eq!(
        file_identity(&fixture.parent.join("guest-control"))?,
        replacement_guest
    );
    assert_eq!(
        std::fs::read(fixture.launch_plan_path())?,
        observed.canonical_bytes
    );
    assert_run_error(&run, "launch plan publication failed");
    Ok(())
}

#[test]
fn foreign_equal_byte_replacement_preserves_original_unlinked_descriptor(
) -> Result<(), Box<dyn Error>> {
    let fixture = ControlFixture::new()?;
    let (spec, controls) = load_controls(&fixture)?;
    let observation = install_post_native_mutation(
        &fixture,
        &controls,
        PublicationMutation::ReplacePlanWithEqualBytes,
    );
    let adapter = Arc::new(PublicationAdapter::new(fixture.cleanup_path()));
    let provider: Arc<dyn GatedProviderAdapter> = adapter.clone();
    let registry = LaunchRetentionRegistry::try_new()?;
    let probe = registry_probe(&registry);
    let _supervisor = probe.hold_supervisor_active_claims();
    let (run, custody) =
        run_publication_custody_fixture(spec, provider, Arc::clone(&registry), fixture.executable)?;
    assert_run_error(&run, "launch plan publication failed");
    let observed = captured_publication(&observation)?;
    let initial = initial_pending(&registry, &controls, &observed)?;
    let foreign_identity = file_identity(&fixture.launch_plan_path())?;
    assert_ne!(
        (foreign_identity.0, foreign_identity.1),
        (observed.identity.0, observed.identity.1)
    );
    let original_unlinked_identity = (observed.identity.0, observed.identity.1, 0);
    assert_eq!(
        pending_publication_file_identity(&registry, initial.ticket),
        Some(original_unlinked_identity)
    );
    assert_pending_unchanged(&fixture, &registry, &controls, &initial, &custody, &adapter);
    assert_eq!(
        probe.advance_one_supervisor_active_attempt()?,
        Some(initial.ticket)
    );
    assert_pending_unchanged(&fixture, &registry, &controls, &initial, &custody, &adapter);
    assert_eq!(
        pending_publication_file_identity(&registry, initial.ticket),
        Some(original_unlinked_identity)
    );
    assert_eq!(
        file_identity(&fixture.launch_plan_path())?,
        foreign_identity
    );
    assert_eq!(
        std::fs::read(fixture.launch_plan_path())?,
        observed.canonical_bytes
    );
    assert_run_error(&run, "launch plan publication failed");
    Ok(())
}

#[test]
fn original_inode_byte_mutation_retains_until_original_bytes_are_genuinely_restored(
) -> Result<(), Box<dyn Error>> {
    let fixture = ControlFixture::new()?;
    let (spec, controls) = load_controls(&fixture)?;
    let observation = install_post_native_mutation(
        &fixture,
        &controls,
        PublicationMutation::MutateOriginalPlanBytes,
    );
    let adapter = Arc::new(PublicationAdapter::new(fixture.cleanup_path()));
    let provider: Arc<dyn GatedProviderAdapter> = adapter.clone();
    let registry = LaunchRetentionRegistry::try_new()?;
    let probe = registry_probe(&registry);
    let _supervisor = probe.hold_supervisor_active_claims();
    let (run, custody) =
        run_publication_custody_fixture(spec, provider, Arc::clone(&registry), fixture.executable)?;
    assert_run_error(&run, "launch plan publication failed");
    let observed = captured_publication(&observation)?;
    let initial = initial_pending(&registry, &controls, &observed)?;
    assert_eq!(std::fs::read(fixture.launch_plan_path())?, FOREIGN_PLAN);
    assert_eq!(
        file_identity(&fixture.launch_plan_path())?,
        observed.identity
    );
    assert_eq!(
        pending_publication_file_identity(&registry, initial.ticket),
        Some(observed.identity)
    );
    assert_pending_unchanged(&fixture, &registry, &controls, &initial, &custody, &adapter);
    assert_eq!(
        probe.advance_one_supervisor_active_attempt()?,
        Some(initial.ticket)
    );
    assert_pending_unchanged(&fixture, &registry, &controls, &initial, &custody, &adapter);
    assert_eq!(std::fs::read(fixture.launch_plan_path())?, FOREIGN_PLAN);

    let mut original = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(fixture.launch_plan_path())?;
    original.write_all(&observed.canonical_bytes)?;
    original.sync_all()?;
    assert_eq!(
        file_identity(&fixture.launch_plan_path())?,
        observed.identity
    );
    assert_eq!(
        probe.advance_one_supervisor_active_attempt()?,
        Some(initial.ticket)
    );
    assert!(active_plan_matches_publication(
        &registry,
        initial.ticket,
        &controls,
        initial.canonical_digest
    ));
    assert_full_custody_held(&probe, &initial, &custody);
    assert!(!fixture.cleanup_path().exists());
    assert_eq!(
        probe.advance_one_supervisor_active_attempt()?,
        Some(initial.ticket)
    );
    assert_eq!(probe.only_ticket(), None);
    assert_eq!(probe.resource_loan_releases(initial.ticket), 1);
    assert_eq!(custody.aggregate_releases(), 1);
    assert_eq!(custody.active_leases(), 0);
    assert!(!custody.gate_directory_exists());
    assert_eq!(PublicationAdapter::count(&adapter.absence_calls), 2);
    assert_no_runtime_actions(&adapter);
    fixture.assert_cli_never_started();
    assert_run_error(&run, "launch plan publication failed");
    Ok(())
}

#[test]
fn definite_foreign_plan_collision_has_no_new_plan_proof_cleanup_or_ticket(
) -> Result<(), Box<dyn Error>> {
    let fixture = ControlFixture::new()?;
    let (spec, controls) = load_controls(&fixture)?;
    let native_success = Arc::new(AtomicBool::new(false));
    let observed_success = Arc::clone(&native_success);
    after_launch_plan_native_publication(
        &controls,
        Box::new(move || {
            observed_success.store(true, Ordering::Release);
        }),
    );
    write_private(&fixture.launch_plan_path(), FOREIGN_PLAN)?;
    let foreign_identity = file_identity(&fixture.launch_plan_path())?;
    let adapter = Arc::new(PublicationAdapter::new(fixture.cleanup_path()));
    let provider: Arc<dyn GatedProviderAdapter> = adapter.clone();
    let registry = LaunchRetentionRegistry::try_new()?;
    let probe = registry_probe(&registry);
    let _supervisor = probe.hold_supervisor_active_claims();
    let (run, custody) =
        run_publication_custody_fixture(spec, provider, Arc::clone(&registry), fixture.executable)?;
    assert_run_error(&run, "existing launch plan requires recovery");
    assert!(!native_success.load(Ordering::Acquire));
    assert!(pending_publication_snapshot(&registry, &controls).is_none());
    assert_eq!(probe.only_ticket(), None);
    assert_eq!(probe.snapshot().retained, 0);
    assert_eq!(probe.advance_one_supervisor_active_attempt()?, None);
    assert_eq!(
        file_identity(&fixture.launch_plan_path())?,
        foreign_identity
    );
    assert_eq!(std::fs::read(fixture.launch_plan_path())?, FOREIGN_PLAN);
    assert!(!fixture.cleanup_path().exists());
    assert!(!fixture.parent.join("launch.json").exists());
    assert_eq!(custody.aggregate_releases(), 1);
    assert_eq!(custody.active_leases(), 0);
    assert!(!custody.gate_directory_exists());
    assert_no_runtime_actions(&adapter);
    fixture.assert_cli_never_started();
    Ok(())
}

#[test]
fn recovery_cleanup_collision_retries_only_publication_and_releases_once(
) -> Result<(), Box<dyn Error>> {
    let fixture = ControlFixture::new()?;
    let (spec, controls) = load_controls(&fixture)?;
    let fault = arm_launch_plan_post_rename_sync_failure(&controls);
    let adapter = Arc::new(PublicationAdapter::new(fixture.cleanup_path()));
    adapter
        .collide_cleanup_on_second_absence
        .store(true, Ordering::Release);
    let provider: Arc<dyn GatedProviderAdapter> = adapter.clone();
    let registry = LaunchRetentionRegistry::try_new()?;
    let probe = registry_probe(&registry);
    let _supervisor = probe.hold_supervisor_active_claims();
    let (run, custody) =
        run_publication_custody_fixture(spec, provider, Arc::clone(&registry), fixture.executable)?;
    assert_run_error(&run, "launch plan publication failed");
    let initial = pending_publication_snapshot(&registry, &controls)
        .ok_or("one genuine pending publication")?;
    let original_plan = std::fs::read(fixture.launch_plan_path())?;
    assert_eq!(initial.canonical_bytes, original_plan);
    assert_pending_unchanged(&fixture, &registry, &controls, &initial, &custody, &adapter);
    fault.clear();
    assert_eq!(
        probe.advance_one_supervisor_active_attempt()?,
        Some(initial.ticket)
    );
    assert!(active_plan_matches_publication(
        &registry,
        initial.ticket,
        &controls,
        initial.canonical_digest
    ));
    let promoted = probe
        .never_created_reconciliation_snapshot(initial.ticket)
        .ok_or("same-ticket genuine never-created promotion")?;
    assert!(promoted.proof_present);
    assert!(!promoted.reconciliation_pending);
    assert_eq!(promoted.resource_loans, 1);
    assert_full_custody_held(&probe, &initial, &custody);
    assert!(!fixture.cleanup_path().exists());
    assert_eq!(PublicationAdapter::count(&adapter.absence_calls), 1);

    assert_eq!(
        probe.advance_one_supervisor_active_attempt()?,
        Some(initial.ticket)
    );
    let retained = probe
        .never_created_reconciliation_snapshot(initial.ticket)
        .ok_or("cleanup publication failure must retain the same ticket")?;
    assert_eq!(
        retained.reason,
        LaunchRetentionReason::PostSpawnControlChanged
    );
    assert!(retained.proof_present);
    assert_eq!(retained.resource_loans, 1);
    assert_eq!(retained.resource_loan_releases, 0);
    assert!(!retained.cleanup_complete);
    assert_full_custody_held(&probe, &initial, &custody);
    assert!(active_plan_matches_publication(
        &registry,
        initial.ticket,
        &controls,
        initial.canonical_digest
    ));
    assert_eq!(PublicationAdapter::count(&adapter.absence_calls), 2);
    assert!(!adapter.cleanup_seen_during_absence.load(Ordering::Acquire));
    assert_eq!(std::fs::read(fixture.cleanup_path())?, FOREIGN_CLEANUP);
    let expected_record = format!(
        "{{\"version\":2,\"launchPlanSha256\":\"{}\",\"outcome\":\"absent\"}}\n",
        lower_hex(&initial.canonical_digest)
    )
    .into_bytes();
    assert_eq!(
        pending_cleanup_record_bytes(&registry, initial.ticket),
        Some(expected_record.clone())
    );
    assert_eq!(std::fs::read(fixture.launch_plan_path())?, original_plan);
    fixture.assert_cli_never_started();
    assert_no_runtime_actions(&adapter);

    std::fs::remove_file(fixture.cleanup_path())?;
    adapter.present.store(true, Ordering::Release);
    assert_eq!(
        probe.advance_one_supervisor_active_attempt()?,
        Some(initial.ticket)
    );
    assert_eq!(probe.only_ticket(), None);
    assert_eq!(
        pending_cleanup_record_bytes(&registry, initial.ticket),
        None
    );
    assert_eq!(std::fs::read(fixture.cleanup_path())?, expected_record);
    let cleanup: serde_json::Value = serde_json::from_slice(&expected_record)?;
    assert_eq!(
        cleanup["launchPlanSha256"],
        lower_hex(&initial.canonical_digest)
    );
    assert!(cleanup.get("runtimeId").is_none());
    assert_eq!(PublicationAdapter::count(&adapter.absence_calls), 2);
    assert_eq!(probe.resource_loan_releases(initial.ticket), 1);
    assert_eq!(custody.aggregate_releases(), 1);
    assert_eq!(custody.active_leases(), 0);
    assert!(!custody.gate_directory_exists());
    fixture.assert_cli_never_started();
    assert_no_runtime_actions(&adapter);
    assert_eq!(std::fs::read(fixture.launch_plan_path())?, original_plan);
    assert_eq!(probe.advance_one_supervisor_active_attempt()?, None);
    assert_eq!(probe.resource_loan_releases(initial.ticket), 1);
    assert_eq!(custody.aggregate_releases(), 1);
    assert_run_error(&run, "launch plan publication failed");
    Ok(())
}
