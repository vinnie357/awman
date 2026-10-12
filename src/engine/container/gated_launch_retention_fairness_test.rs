use std::error::Error;
use std::io::Write as _;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use sha2::{Digest, Sha256};

use super::gated_launch::child_lifecycle_test_support::{lifecycle_probe, LifecycleProbe};
use super::gated_launch::retention_test_support::{registry_probe, RetentionProbe};
use super::gated_launch::{
    canonical_inspection_revision, retain_detached_after_error, AbsenceObservation,
    ActiveGatedCustody, AppleProviderState, CanonicalInspectionRevisionInput,
    ChildLifecycleAuthority, CreateCliExitClass, CreateCliExitEvidence, DurableLaunchPlan,
    ExactInspection, GatedProviderAdapter, ImmutableImageId, InspectionObservationKind,
    LaunchIdentityError, LaunchRetentionRegistry, LaunchRetentionTicket, NamePresence,
    ProviderCallDeadline, ProviderKind, ProviderLaunchInspection, ProviderLaunchKey, ProviderState,
    SanitizedProviderStateObservation, SpawnedCreateCli,
};
use super::options::{ContainerName, ImageRef};
use crate::data::startup_gate::load_startup_gate;
use crate::engine::error::EngineError;

const IMAGE: &str = "sha256:cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd";

struct CleanupAdapter {
    runtime_id: &'static str,
    resolve_calls: AtomicUsize,
    absence_calls: AtomicUsize,
    cleanup_authorized: AtomicBool,
    inspect_attempts: AtomicUsize,
    qualified_inspections: AtomicUsize,
    stop_calls: AtomicUsize,
    remove_calls: AtomicUsize,
}

impl CleanupAdapter {
    fn new(runtime_id: &'static str, cleanup_authorized: bool) -> Self {
        Self {
            runtime_id,
            resolve_calls: AtomicUsize::new(0),
            absence_calls: AtomicUsize::new(0),
            cleanup_authorized: AtomicBool::new(cleanup_authorized),
            inspect_attempts: AtomicUsize::new(0),
            qualified_inspections: AtomicUsize::new(0),
            stop_calls: AtomicUsize::new(0),
            remove_calls: AtomicUsize::new(0),
        }
    }

    fn authorize_cleanup(&self) {
        self.cleanup_authorized.store(true, Ordering::Release);
    }

    fn matching(
        &self,
        key: &ProviderLaunchKey,
        state: AppleProviderState,
    ) -> ProviderLaunchInspection {
        let state = ProviderState::Apple(state);
        ProviderLaunchInspection {
            provider: key.provider,
            runtime_id: self.runtime_id.into(),
            exact_name: key.container_name.clone(),
            token_digest: key.token_digest,
            immutable_image_id: key.immutable_image_id.clone(),
            created_at: key.created_not_before,
            revision: canonical_inspection_revision(CanonicalInspectionRevisionInput {
                kind: InspectionObservationKind::Matching,
                provider: key.provider,
                exact_name: &key.container_name,
                runtime_id: Some(self.runtime_id),
                token_digest: Some(key.token_digest),
                immutable_image_id: Some(&key.immutable_image_id),
                created_at: Some(key.created_not_before),
                state: SanitizedProviderStateObservation::Known(state.clone()),
            }),
            state,
        }
    }

    fn count(counter: &AtomicUsize) -> usize {
        counter.load(Ordering::Acquire)
    }
}

impl GatedProviderAdapter for CleanupAdapter {
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
        key: &ProviderLaunchKey,
        _deadline: ProviderCallDeadline,
    ) -> ExactInspection {
        self.inspect_attempts.fetch_add(1, Ordering::AcqRel);
        if !self.cleanup_authorized.load(Ordering::Acquire) {
            return ExactInspection::Unavailable;
        }
        let state = if self.qualified_inspections.fetch_add(1, Ordering::AcqRel) == 0 {
            AppleProviderState::Running
        } else {
            AppleProviderState::Stopped
        };
        ExactInspection::Matching(self.matching(key, state))
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
        for (name, bytes) in [
            ("manifest.json", manifest.as_slice()),
            ("request.json", request.as_bytes()),
            ("launch-intent.json", intent.as_bytes()),
        ] {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(parent.join(name))?;
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
        Ok(super::gated_launch::prepare_orchestrated_launch(
            adapter,
            &ImageRef::new("awman-retention-fairness:latest"),
            identity,
            Arc::clone(controls),
            Instant::now() + Duration::from_secs(4),
        )?)
    }
}

fn lower_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn active_custody(
    plan: DurableLaunchPlan,
    adapter: Arc<CleanupAdapter>,
) -> Result<(ActiveGatedCustody, LifecycleProbe), Box<dyn Error>> {
    let prepared = ChildLifecycleAuthority::prepare()?;
    let child = Command::new("/bin/cat")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let lifecycle = match prepared.bind_started_child_with_resources(
        SpawnedCreateCli::Piped(child),
        Utc::now(),
        Box::new(()),
    ) {
        Ok(lifecycle) => lifecycle,
        Err(error) => {
            retain_detached_after_error(error.into_retained());
            return Err("fairness fixture failed to bind child custody".into());
        }
    };
    let probe = lifecycle_probe(&lifecycle);
    let (_, resource_loan) = match lifecycle.transform_resources::<(), _>(|_| Ok(())) {
        Ok(transformed) => transformed,
        Err(error) => {
            let _ = lifecycle.terminate_local_cli(Instant::now() + Duration::from_secs(2));
            return Err(error.into());
        }
    };
    let provider: Arc<dyn GatedProviderAdapter> = adapter;
    Ok((
        ActiveGatedCustody::new(plan, lifecycle, resource_loan, provider, None, None),
        probe,
    ))
}

fn ticket_state(probe: &RetentionProbe, ticket: LaunchRetentionTicket) -> (bool, usize, usize) {
    (
        probe.contains(ticket),
        probe.active_gated_resource_loans(ticket),
        probe.resource_loan_releases(ticket),
    )
}

#[test]
fn automatic_supervisor_rotates_past_retained_oldest_ticket_and_releases_later_custody_once(
) -> Result<(), Box<dyn Error>> {
    let registry = LaunchRetentionRegistry::try_new()
        .map_err(|_| "launch retention registry failed to start")?;
    let probe = registry_probe(&registry);
    let supervisor = probe.hold_supervisor_active_claims();
    let oldest_adapter = Arc::new(CleanupAdapter::new("retention-fairness-oldest", false));
    let later_adapter = Arc::new(CleanupAdapter::new("retention-fairness-later", true));
    let oldest_controls = ControlFixture::new(
        "awman-altana-retention-fairness-oldest",
        "3131313131313131313131313131313131313131313131313131313131313131",
    )?;
    let later_controls = ControlFixture::new(
        "awman-altana-retention-fairness-later",
        "3232323232323232323232323232323232323232323232323232323232323232",
    )?;
    let oldest_plan = oldest_controls.plan(oldest_adapter.as_ref())?;
    let later_plan = later_controls.plan(later_adapter.as_ref())?;
    let (oldest_custody, oldest_lifecycle) =
        active_custody(oldest_plan, Arc::clone(&oldest_adapter))?;
    let (later_custody, later_lifecycle) =
        match active_custody(later_plan, Arc::clone(&later_adapter)) {
            Ok(custody) => custody,
            Err(setup_error) => {
                oldest_adapter.authorize_cleanup();
                let oldest = registry.retain_active_gated(oldest_custody);
                let cleanup = probe.advance_one_supervisor_active_attempt();
                drop(supervisor);
                drop(registry);
                return match cleanup {
                    Ok(Some(selected)) if selected == oldest => Err(setup_error),
                    Ok(_) => Err(
                        "fairness fixture could not clean oldest custody after later setup failure"
                            .into(),
                    ),
                    Err(cleanup_error) => Err(cleanup_error.into()),
                };
            }
        };
    let oldest = registry.retain_active_gated(oldest_custody);
    let later = registry.retain_active_gated(later_custody);
    let initial = (ticket_state(&probe, oldest), ticket_state(&probe, later));

    let first_attempt = probe.advance_one_supervisor_active_attempt();
    let after_first = (ticket_state(&probe, oldest), ticket_state(&probe, later));
    let second_attempt = probe.advance_one_supervisor_active_attempt();
    let after_second = (ticket_state(&probe, oldest), ticket_state(&probe, later));
    let adapter_after_second = (
        CleanupAdapter::count(&oldest_adapter.inspect_attempts),
        CleanupAdapter::count(&later_adapter.inspect_attempts),
        CleanupAdapter::count(&later_adapter.stop_calls),
        CleanupAdapter::count(&later_adapter.remove_calls),
        CleanupAdapter::count(&oldest_adapter.resolve_calls),
        CleanupAdapter::count(&later_adapter.resolve_calls),
        CleanupAdapter::count(&oldest_adapter.absence_calls),
        CleanupAdapter::count(&later_adapter.absence_calls),
    );
    let oldest_after_second = oldest_lifecycle.snapshot();
    let later_after_second = later_lifecycle.snapshot();

    oldest_adapter.authorize_cleanup();
    let cleanup_oldest = probe
        .contains(oldest)
        .then(|| probe.advance_one_supervisor_active_attempt());
    let cleanup_later = probe
        .contains(later)
        .then(|| probe.advance_one_supervisor_active_attempt());
    let final_state = (ticket_state(&probe, oldest), ticket_state(&probe, later));
    let oldest_final = oldest_lifecycle.snapshot();
    let later_final = later_lifecycle.snapshot();
    let adapter_final = (
        CleanupAdapter::count(&oldest_adapter.resolve_calls),
        CleanupAdapter::count(&later_adapter.resolve_calls),
        CleanupAdapter::count(&oldest_adapter.absence_calls),
        CleanupAdapter::count(&later_adapter.absence_calls),
    );
    drop(supervisor);
    drop(registry);

    assert_eq!(initial, ((true, 1, 0), (true, 1, 0)));
    assert_eq!(first_attempt?, Some(oldest));
    assert_eq!(after_first, ((true, 1, 0), (true, 1, 0)));
    assert_eq!(second_attempt?, Some(later));
    assert_eq!(after_second, ((true, 1, 0), (false, 0, 1)));
    assert_eq!(adapter_after_second, (1, 2, 1, 1, 1, 1, 1, 2));
    assert_eq!(oldest_after_second.native_reaps, 0);
    assert!(oldest_after_second.resources_present);
    assert_eq!(later_after_second.native_reaps, 1);
    assert!(later_after_second.bound_pid.is_some());
    assert!(!later_after_second.resources_present);
    assert_eq!(cleanup_oldest.transpose()?, Some(Some(oldest)));
    assert_eq!(cleanup_later.transpose()?, None);
    assert_eq!(final_state, ((false, 0, 1), (false, 0, 1)));
    assert!(oldest_final.bound_pid.is_some());
    assert_eq!(oldest_final.native_reaps, 1);
    assert_eq!(oldest_final.resource_retain_calls, 1);
    assert_eq!(later_final.bound_pid, later_after_second.bound_pid);
    assert_eq!(later_final.native_reaps, 1);
    assert_eq!(later_final.resource_retain_calls, 1);
    assert_ne!(oldest_final.bound_pid, later_final.bound_pid);
    assert_eq!(adapter_final, (1, 1, 2, 2));
    Ok(())
}
