use std::error::Error;
use std::io::Write as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use sha2::{Digest, Sha256};

use super::gated_launch::retention_test_support::{
    active_plan_matches_publication, pending_publication_snapshot, registry_probe,
    ticket_is_pending,
};
use super::gated_launch::{
    canonical_inspection_revision, AbsenceObservation, CanonicalInspectionRevisionInput,
    CreateCliExitClass, CreateCliExitEvidence, ExactInspection, GatedProviderAdapter,
    ImmutableImageId, InspectionObservationKind, LaunchIdentityError, LaunchRetentionReason,
    LaunchRetentionRegistry, NamePresence, ProviderCallDeadline, ProviderKind,
    ProviderLaunchInspection, SanitizedProviderStateObservation,
};
use super::options::{ContainerName, ImageRef};
use super::process::test_support::run_publication_custody_fixture;
use crate::data::startup_gate::{arm_launch_plan_post_rename_sync_failure, load_startup_gate};
use crate::engine::error::EngineError;

const IMAGE: &str = "sha256:abababababababababababababababababababababababababababababababab";
const NAME: &str = "awman-altana-publication-custody";

struct PublicationAdapter {
    cleanup_path: PathBuf,
    absence_calls: AtomicUsize,
    launch_inspections: AtomicUsize,
    stop_calls: AtomicUsize,
    remove_calls: AtomicUsize,
    cleanup_seen_during_absence: AtomicBool,
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
        self.absence_calls.fetch_add(1, Ordering::AcqRel);
        if self.cleanup_path.exists() {
            self.cleanup_seen_during_absence
                .store(true, Ordering::Release);
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

#[test]
fn post_rename_sync_failure_retains_real_run_custody_until_same_ticket_cleanup(
) -> Result<(), Box<dyn Error>> {
    let fixture = ControlFixture::new()?;
    let startup_gate = load_startup_gate(&fixture.parent, Duration::from_secs(30))?;
    let expected_controls = Arc::clone(
        startup_gate
            .control
            .orchestrated_parts()
            .ok_or("expected orchestrated controls")?
            .0,
    );
    let fault = arm_launch_plan_post_rename_sync_failure(&expected_controls);
    let adapter = Arc::new(PublicationAdapter::new(fixture.cleanup_path()));
    let provider: Arc<dyn GatedProviderAdapter> = adapter.clone();
    let registry = LaunchRetentionRegistry::try_new()?;
    let probe = registry_probe(&registry);
    let _supervisor_claim_barrier = probe.hold_supervisor_active_claims();

    let (run, custody) = run_publication_custody_fixture(
        startup_gate,
        provider,
        Arc::clone(&registry),
        fixture.executable,
    )?;
    let error = match run {
        Ok(_) => return Err("publication uncertainty unexpectedly started the CLI".into()),
        Err(error) => error,
    };
    match error {
        EngineError::Container(message) => {
            assert_eq!(message, "launch plan publication failed");
        }
        other => return Err(format!("unexpected publication error: {other:?}").into()),
    }

    let plan_metadata = std::fs::symlink_metadata(fixture.launch_plan_path())?;
    assert!(plan_metadata.file_type().is_file());
    assert_eq!(plan_metadata.mode() & 0o777, 0o600);
    let plan_bytes = std::fs::read(fixture.launch_plan_path())?;
    let initial = pending_publication_snapshot(&registry, &expected_controls)
        .ok_or("one pending publication snapshot")?;
    assert_eq!(probe.only_ticket(), Some(initial.ticket));
    assert!(ticket_is_pending(&registry, initial.ticket));
    assert_eq!(initial.canonical_bytes, plan_bytes);
    let actual_digest: [u8; 32] = Sha256::digest(&plan_bytes).into();
    assert_eq!(initial.canonical_digest, actual_digest);
    assert!(initial.same_controls);
    assert!(initial.prepared_present);
    assert!(initial.resource_bundle_present);
    assert_eq!(probe.resource_loan_releases(initial.ticket), 0);
    assert_eq!(custody.aggregate_releases(), 0);
    assert_eq!(custody.active_leases(), 1);
    assert!(custody.gate_directory_exists());
    fixture.assert_cli_never_started();
    assert!(!fixture.cleanup_path().exists());
    assert_eq!(PublicationAdapter::count(&adapter.absence_calls), 1);
    assert_no_runtime_actions(&adapter);

    assert_eq!(
        probe.advance_one_supervisor_active_attempt()?,
        Some(initial.ticket)
    );
    assert_eq!(
        pending_publication_snapshot(&registry, &expected_controls),
        Some(initial.clone())
    );
    assert_eq!(custody.aggregate_releases(), 0);
    assert_eq!(custody.active_leases(), 1);
    assert!(custody.gate_directory_exists());
    fixture.assert_cli_never_started();
    assert!(!fixture.cleanup_path().exists());
    assert_eq!(PublicationAdapter::count(&adapter.absence_calls), 1);
    assert_no_runtime_actions(&adapter);

    fault.clear();
    assert_eq!(
        probe.advance_one_supervisor_active_attempt()?,
        Some(initial.ticket)
    );
    assert!(!ticket_is_pending(&registry, initial.ticket));
    assert!(active_plan_matches_publication(
        &registry,
        initial.ticket,
        &expected_controls,
        initial.canonical_digest,
    ));
    let promoted = probe
        .never_created_reconciliation_snapshot(initial.ticket)
        .ok_or("same-ticket never-created promotion")?;
    assert_eq!(promoted.ticket, initial.ticket);
    assert_eq!(promoted.reason, LaunchRetentionReason::SpawnResultUnknown);
    assert!(!promoted.reconciliation_pending);
    assert!(promoted.proof_present);
    assert_eq!(promoted.resource_loans, 1);
    assert_eq!(promoted.resource_loan_releases, 0);
    assert!(!promoted.cleanup_complete);
    assert_eq!(custody.aggregate_releases(), 0);
    assert_eq!(custody.active_leases(), 1);
    assert!(custody.gate_directory_exists());
    fixture.assert_cli_never_started();
    assert!(!fixture.cleanup_path().exists());
    assert_eq!(PublicationAdapter::count(&adapter.absence_calls), 1);
    assert_no_runtime_actions(&adapter);

    assert_eq!(
        probe.advance_one_supervisor_active_attempt()?,
        Some(initial.ticket)
    );
    assert_eq!(probe.only_ticket(), None);
    assert_eq!(probe.resource_loan_releases(initial.ticket), 1);
    assert_eq!(custody.aggregate_releases(), 1);
    assert_eq!(custody.active_leases(), 0);
    assert!(!custody.gate_directory_exists());
    fixture.assert_cli_never_started();
    assert_eq!(PublicationAdapter::count(&adapter.absence_calls), 2);
    assert_no_runtime_actions(&adapter);
    assert!(!adapter.cleanup_seen_during_absence.load(Ordering::Acquire));

    let cleanup: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixture.cleanup_path())?)?;
    assert_eq!(
        cleanup,
        serde_json::json!({
            "version": 2,
            "launchPlanSha256": lower_hex(&initial.canonical_digest),
            "outcome": "absent"
        })
    );
    assert_eq!(std::fs::read(fixture.launch_plan_path())?, plan_bytes);
    Ok(())
}
