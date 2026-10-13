use super::gated_launch::child_lifecycle_test_support::lifecycle_probe;
use super::gated_launch::{
    canonical_inspection_revision, prepare_orchestrated_launch, AbsenceObservation,
    AppleProviderState, CanonicalInspectionRevisionInput, CreateCliExitClass,
    CreateCliExitEvidence, DurableLaunchPlan, ExactInspection, GatedProviderAdapter,
    ImmutableImageId, InspectionObservationKind, LaunchIdentityError, LaunchRetentionRegistry,
    NamePresence, ProviderCallDeadline, ProviderKind, ProviderLaunchInspection, ProviderState,
    SanitizedProviderStateObservation,
};
use super::options::{ContainerName, ImageRef};
use super::process::test_support::{
    spawn_fixture_with_custody_witness, FixtureIoMode, FixtureSpawnGate, FixtureSpawnSpec,
};
use crate::data::startup_gate::load_startup_gate;
use crate::engine::error::EngineError;
use chrono::Utc;
use sha2::{Digest, Sha256};
use std::os::unix::fs::PermissionsExt as _;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use std::{error::Error, fs};
const NAME: &str = "awman-altana-gate-rejection";
const IMAGE: &str = "sha256:abababababababababababababababababababababababababababababababab";
const BOUND: Duration = Duration::from_secs(4);
#[derive(Default)]
struct CleanupAdapter {
    created: AtomicBool,
    stopped: AtomicBool,
    removed: AtomicBool,
    created_at: OnceLock<chrono::DateTime<Utc>>,
    inspections: AtomicUsize,
    absences: AtomicUsize,
    stops: AtomicUsize,
    removes: AtomicUsize,
}
impl CleanupAdapter {
    fn matching(&self, key: &super::gated_launch::ProviderLaunchKey) -> ExactInspection {
        let state = ProviderState::Apple(if self.stopped.load(Ordering::Acquire) {
            AppleProviderState::Stopped
        } else {
            AppleProviderState::Running
        });
        let runtime_id = "aa".repeat(32);
        let created_at = self.created_at.get().copied().expect("created identity");
        ExactInspection::Matching(ProviderLaunchInspection {
            provider: ProviderKind::AppleContainers,
            runtime_id: runtime_id.clone(),
            exact_name: key.container_name.clone(),
            token_digest: key.token_digest,
            immutable_image_id: key.immutable_image_id.clone(),
            created_at,
            revision: canonical_inspection_revision(CanonicalInspectionRevisionInput {
                kind: InspectionObservationKind::Matching,
                provider: ProviderKind::AppleContainers,
                exact_name: &key.container_name,
                runtime_id: Some(&runtime_id),
                token_digest: Some(key.token_digest),
                immutable_image_id: Some(&key.immutable_image_id),
                created_at: Some(created_at),
                state: SanitizedProviderStateObservation::Known(state.clone()),
            }),
            state,
        })
    }
    fn absence(&self, name: &ContainerName) -> AbsenceObservation {
        AbsenceObservation {
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
        }
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
        ImmutableImageId::new(IMAGE)
    }
    fn inspect_name_absence(
        &self,
        name: &ContainerName,
        _deadline: ProviderCallDeadline,
    ) -> Result<AbsenceObservation, NamePresence> {
        self.absences.fetch_add(1, Ordering::AcqRel);
        if self.created.load(Ordering::Acquire) && !self.removed.load(Ordering::Acquire) {
            Err(NamePresence::Present)
        } else {
            Ok(self.absence(name))
        }
    }
    fn inspect_launch(
        &self,
        key: &super::gated_launch::ProviderLaunchKey,
        _deadline: ProviderCallDeadline,
    ) -> ExactInspection {
        self.inspections.fetch_add(1, Ordering::AcqRel);
        self.matching(key)
    }
    fn classify_create_exit(&self, _exit: &CreateCliExitEvidence) -> CreateCliExitClass {
        CreateCliExitClass::Other
    }
    fn stop_inspected(
        &self,
        _inspection: &ProviderLaunchInspection,
        _deadline: ProviderCallDeadline,
    ) -> Result<(), EngineError> {
        self.stops.fetch_add(1, Ordering::AcqRel);
        self.stopped.store(true, Ordering::Release);
        Ok(())
    }
    fn remove_inspected(
        &self,
        _inspection: &ProviderLaunchInspection,
        _deadline: ProviderCallDeadline,
    ) -> Result<(), EngineError> {
        self.removes.fetch_add(1, Ordering::AcqRel);
        self.removed.store(true, Ordering::Release);
        Ok(())
    }
}
struct ControlFixture {
    _temp: tempfile::TempDir,
    parent: std::path::PathBuf,
}
impl ControlFixture {
    fn new() -> Result<Self, Box<dyn Error>> {
        let temp = tempfile::tempdir()?;
        let parent = temp.path().join("control");
        let guest = parent.join("guest-control");
        fs::create_dir(&parent)?;
        fs::create_dir(&guest)?;
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700))?;
        fs::set_permissions(&guest, fs::Permissions::from_mode(0o700))?;
        let manifest = b"{\"version\":1,\"entries\":[]}\n";
        let digest: String = Sha256::digest(manifest)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let request = format!("{{\"version\":1,\"bindings\":[{{\"id\":\"workspace\",\"workspace_path\":\"/workspace\",\"manifest_id\":\"{digest}\",\"manifest_file\":\"workspace.manifest.json\",\"access\":\"read-only\"}}]}}\n");
        let intent = format!(
            "{{\"version\":1,\"container_name\":\"{NAME}\",\"launch_token\":\"{}\"}}\n",
            "11".repeat(32)
        );
        for (name, bytes) in [
            ("workspace.manifest.json", manifest.as_slice()),
            ("request.json", request.as_bytes()),
            ("launch-intent.json", intent.as_bytes()),
        ] {
            fs::write(parent.join(name), bytes)?;
            fs::set_permissions(parent.join(name), fs::Permissions::from_mode(0o600))?;
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
            .ok_or("orchestrated controls")?;
        Ok(prepare_orchestrated_launch(
            adapter,
            &ImageRef::new("awman-gate-rejection:local"),
            identity,
            Arc::clone(controls),
            Instant::now() + BOUND,
        )?)
    }
    fn publish_invalid_ready(&self, plan: &DurableLaunchPlan) -> Result<(), Box<dyn Error>> {
        let binding = &plan.controls.request.request.bindings[0];
        let ready = serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "nonce": "22".repeat(32),
            "container_name": "awman-altana-foreign",
            "bindings": [{
                "id": binding.id.clone(),
                "manifest_id": binding.manifest_id.clone(),
                "access": binding.access,
                "workspace_path": binding.workspace_path.clone(),
            }],
        }))?;
        plan.controls
            .publish_guest_record("ready.json", ".ready", &ready)?;
        Ok(())
    }
}
fn wait_until(deadline: Instant, mut condition: impl FnMut() -> bool) -> bool {
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    condition()
}
fn lower_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_gate_stays_failed_after_reaped_zero_exit_and_successful_cleanup(
) -> Result<(), Box<dyn Error>> {
    let controls = ControlFixture::new()?;
    let adapter = Arc::new(CleanupAdapter::default());
    let plan = controls.plan(adapter.as_ref())?;
    assert!(controls.parent.join("launch-plan.json").is_file());
    let registry = LaunchRetentionRegistry::try_new()?;
    let provider: Arc<dyn GatedProviderAdapter> = adapter.clone();
    let (spawn, witness) = spawn_fixture_with_custody_witness(
        FixtureSpawnSpec {
            mode: FixtureIoMode::Piped,
            executable: "/bin/sh",
            args: vec!["-c".into(), "exit 0".into()],
            seeded_prompt: None,
            grace_timeout: Duration::from_millis(50),
            stuck_timeout: Duration::from_millis(50),
            gate: Some(FixtureSpawnGate {
                plan: plan.clone(),
                adapter: provider,
                enclosing: Instant::now() + BOUND,
            }),
            fault: None,
        },
        Arc::clone(&registry),
    );
    assert!(
        spawn.result.is_ok(),
        "fixture must reach the real wait path: {:?}",
        spawn.result.as_ref().err()
    );
    let absence_calls_after_spawn = adapter.absences.load(Ordering::Acquire);
    let _ = adapter.created_at.set(Utc::now());
    adapter.created.store(true, Ordering::Release);
    controls.publish_invalid_ready(&plan)?;
    let authority = spawn.slot.authority().ok_or("bound lifecycle authority")?;
    let lifecycle = lifecycle_probe(&authority);
    let mut execution = spawn.finish(registry)?;
    assert!(wait_until(Instant::now() + BOUND, || lifecycle
        .snapshot()
        .actual_exit
        .is_some_and(|exit| exit.exit_code == 0)));
    let wait_result = execution.wait().await;
    assert_eq!(adapter.inspections.load(Ordering::Acquire), 2);
    assert_eq!(adapter.stops.load(Ordering::Acquire), 1);
    assert_eq!(adapter.removes.load(Ordering::Acquire), 1);
    assert_eq!(
        adapter.absences.load(Ordering::Acquire),
        absence_calls_after_spawn + 1
    );
    assert_eq!(witness.releases(), 1);
    let cleanup: serde_json::Value =
        serde_json::from_slice(&fs::read(controls.parent.join("cleanup.json"))?)?;
    assert_eq!(
        cleanup,
        serde_json::json!({"version": 2, "outcome": "absent", "runtimeId": "aa".repeat(32), "launchPlanSha256": lower_hex(&Sha256::digest(fs::read(controls.parent.join("launch-plan.json"))?))})
    );
    assert!(!controls.parent.join("guest-control/release.json").exists());
    assert!(!controls
        .parent
        .join("guest-control/provider-entry.marker")
        .exists());
    assert!(
        wait_result.is_err(),
        "gate rejection must survive successful cleanup"
    );
    Ok(())
}
