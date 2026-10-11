use std::error::Error;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use sha2::{Digest, Sha256};
use tempfile::TempDir;

use super::gated_launch::child_lifecycle_test_support::{lifecycle_probe, LifecycleProbe};
use super::gated_launch::retention_test_support::{registry_probe, RetentionProbe};
use super::gated_launch::{
    canonical_inspection_revision, prepare_orchestrated_launch, AbsenceObservation,
    CanonicalInspectionRevisionInput, ChildLifecycleAuthority, CreateCliExitClass,
    CreateCliExitEvidence, DurableLaunchPlan, ExactInspection, GatedProviderAdapter,
    ImmutableImageId, InspectionObservationKind, LaunchIdentityError, LaunchRetentionRegistry,
    NamePresence, ProviderCallDeadline, ProviderKind, ProviderLaunchInspection,
    SanitizedProviderStateObservation,
};
use super::options::{ContainerName, ImageRef};
use super::process::test_support::{
    spawn_fixture, FixtureIoMode, FixtureSpawnFault, FixtureSpawnGate, FixtureSpawnSpec,
};
use crate::data::startup_gate::load_startup_gate;
use crate::engine::error::EngineError;

const OBSERVATION_BOUND: Duration = Duration::from_millis(600);
const FIRST_POLL_BOUND: Duration = Duration::from_millis(200);
const REAP_BOUND: Duration = Duration::from_secs(3);

#[derive(Default)]
struct RecordingAdapter {
    stop_calls: AtomicUsize,
    remove_calls: AtomicUsize,
}

impl RecordingAdapter {
    fn destructive_calls(&self) -> (usize, usize) {
        (
            self.stop_calls.load(Ordering::Acquire),
            self.remove_calls.load(Ordering::Acquire),
        )
    }
}

impl GatedProviderAdapter for RecordingAdapter {
    fn provider(&self) -> ProviderKind {
        ProviderKind::AppleContainers
    }

    fn resolve_image_identity(
        &self,
        _image: &ImageRef,
        _deadline: ProviderCallDeadline,
    ) -> Result<ImmutableImageId, LaunchIdentityError> {
        ImmutableImageId::new(format!("sha256:{}", "1".repeat(64)))
    }

    fn inspect_name_absence(
        &self,
        name: &ContainerName,
        _deadline: ProviderCallDeadline,
    ) -> Result<AbsenceObservation, NamePresence> {
        let checked_at = Utc::now();
        Ok(AbsenceObservation {
            provider: ProviderKind::AppleContainers,
            exact_name: name.clone(),
            checked_at,
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

struct GatedFixture {
    _control: TempDir,
    plan: DurableLaunchPlan,
    adapter: Arc<RecordingAdapter>,
}

fn private_write(path: &Path, bytes: &[u8]) -> Result<(), Box<dyn Error>> {
    fs::write(path, bytes)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

fn gated_fixture(tag: &str) -> Result<GatedFixture, Box<dyn Error>> {
    let control = tempfile::tempdir()?;
    fs::set_permissions(control.path(), fs::Permissions::from_mode(0o700))?;
    let guest = control.path().join("guest-control");
    fs::create_dir(&guest)?;
    fs::set_permissions(&guest, fs::Permissions::from_mode(0o700))?;

    let manifest = br#"{"version":1,"entries":[]}"#;
    let manifest_digest: String = Sha256::digest(manifest)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    private_write(&control.path().join("workspace.manifest.json"), manifest)?;
    private_write(
        &control.path().join("request.json"),
        &serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "bindings": [{
                "id": "workspace",
                "workspace_path": "/workspace",
                "manifest_id": manifest_digest,
                "manifest_file": "workspace.manifest.json",
                "access": "read-write"
            }]
        }))?,
    )?;
    private_write(
        &control.path().join("launch-intent.json"),
        &serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "container_name": format!("awman-altana-{tag}"),
            "launch_token": "11".repeat(32)
        }))?,
    )?;

    let spec = load_startup_gate(control.path(), Duration::from_secs(30))?;
    let (controls, identity) = spec
        .control
        .orchestrated_parts()
        .ok_or("orchestrated fixture did not yield held controls")?;
    let adapter = Arc::new(RecordingAdapter::default());
    let plan = prepare_orchestrated_launch(
        adapter.as_ref(),
        &ImageRef::new("awman-quiescence:local"),
        identity,
        Arc::clone(controls),
        Instant::now() + Duration::from_secs(5),
    )?;
    Ok(GatedFixture {
        _control: control,
        plan,
        adapter,
    })
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

struct ManagedFixtureRelease {
    authority: Option<ChildLifecycleAuthority>,
}

impl ManagedFixtureRelease {
    fn new(authority: ChildLifecycleAuthority) -> Self {
        Self {
            authority: Some(authority),
        }
    }

    fn disarm(mut self) {
        drop(self.authority.take());
    }
}

impl Drop for ManagedFixtureRelease {
    fn drop(&mut self) {
        if let Some(authority) = self.authority.take() {
            let _ = authority.terminate_local_cli(Instant::now() + Duration::from_secs(2));
        }
    }
}

type ManagedFixture = (
    Arc<LaunchRetentionRegistry>,
    RetentionProbe,
    Arc<super::gated_launch::ChildLifecycleSlot>,
    LifecycleProbe,
    ManagedFixtureRelease,
    Arc<RecordingAdapter>,
    TempDir,
    std::path::PathBuf,
);

fn managed_fixture(mode: FixtureIoMode, tag: &str) -> Result<ManagedFixture, Box<dyn Error>> {
    let gated = gated_fixture(tag)?;
    let plan_path = gated._control.path().join("launch-plan.json");
    let registry = LaunchRetentionRegistry::try_new()?;
    let registry_observation = registry_probe(&registry);
    let adapter_trait: Arc<dyn GatedProviderAdapter> = gated.adapter.clone();
    let fixture = spawn_fixture(
        FixtureSpawnSpec {
            mode,
            executable: "/bin/sh",
            args: vec!["-c".into(), "exec sleep 30".into()],
            seeded_prompt: None,
            grace_timeout: Duration::from_millis(50),
            stuck_timeout: Duration::from_millis(50),
            gate: Some(FixtureSpawnGate {
                plan: gated.plan,
                adapter: adapter_trait,
                enclosing: Instant::now() + Duration::from_secs(5),
            }),
            fault: Some(FixtureSpawnFault::AfterBindBeforeBridge),
        },
        Arc::clone(&registry),
    );
    let slot = Arc::clone(&fixture.slot);
    let authority = slot
        .authority()
        .ok_or("managed fixture did not bind lifecycle authority")?;
    let lifecycle_observation = lifecycle_probe(&authority);
    let release = ManagedFixtureRelease::new(authority);
    assert!(fixture.finish(Arc::clone(&registry)).is_err());
    Ok((
        registry,
        registry_observation,
        slot,
        lifecycle_observation,
        release,
        gated.adapter,
        gated._control,
        plan_path,
    ))
}

#[test]
fn retained_bind_fault_quiesces_each_managed_cli_representation_once() -> Result<(), Box<dyn Error>>
{
    let modes = [
        FixtureIoMode::Pty { cols: 80, rows: 24 },
        FixtureIoMode::Piped,
        FixtureIoMode::PersistentPiped,
    ];
    for (index, mode) in modes.into_iter().enumerate() {
        let (_registry, retention, slot, lifecycle, release, adapter, _control, plan_path) =
            managed_fixture(mode, &format!("quiesce-{index}"))?;
        let release = release;
        let ticket = retention
            .only_ticket()
            .ok_or("fixture did not retain exactly one gated launch")?;

        assert!(wait_until(Instant::now() + OBSERVATION_BOUND, || {
            let snapshot = lifecycle.snapshot();
            snapshot.terminate_calls == 1 && snapshot.terminate_commands == 1
        }));
        assert!(wait_until(Instant::now() + REAP_BOUND, || {
            lifecycle.snapshot().actual_exit.is_some() && retention.snapshot().unreaped == 0
        }));

        let lifecycle_snapshot = lifecycle.snapshot();
        assert_eq!(lifecycle_snapshot.terminate_calls, 1);
        assert_eq!(lifecycle_snapshot.terminate_commands, 1);
        assert_eq!(lifecycle_snapshot.native_reaps, 1);
        assert!(!lifecycle_snapshot.owns_unreaped_child);
        assert!(lifecycle_snapshot.resources_present);
        let retained = retention.snapshot();
        assert_eq!(retained.retained, 1);
        assert_eq!(retained.unreaped, 0);
        assert!(retention.contains(ticket));
        assert!(plan_path.exists());
        assert_eq!(adapter.destructive_calls(), (0, 0));

        release.disarm();
        drop(slot);
        assert!(wait_until(Instant::now() + OBSERVATION_BOUND, || {
            let snapshot = lifecycle.snapshot();
            snapshot.actor_finished && !snapshot.resources_present
        }));
    }
    Ok(())
}

#[test]
fn retained_managed_termination_timeout_keeps_custody_without_retry_or_fake_exit(
) -> Result<(), Box<dyn Error>> {
    let gated = gated_fixture("timeout")?;
    let plan_path = gated._control.path().join("launch-plan.json");
    let registry = LaunchRetentionRegistry::try_new()?;
    let retention = registry_probe(&registry);
    let adapter_trait: Arc<dyn GatedProviderAdapter> = gated.adapter.clone();
    let fixture = spawn_fixture(
        FixtureSpawnSpec {
            mode: FixtureIoMode::Piped,
            executable: "/bin/sh",
            args: vec!["-c".into(), "exec sleep 30".into()],
            seeded_prompt: None,
            grace_timeout: Duration::from_millis(50),
            stuck_timeout: Duration::from_millis(50),
            gate: Some(FixtureSpawnGate {
                plan: gated.plan,
                adapter: adapter_trait,
                enclosing: Instant::now() + Duration::from_secs(5),
            }),
            fault: Some(FixtureSpawnFault::AfterBindBeforeBridge),
        },
        Arc::clone(&registry),
    );
    let slot = Arc::clone(&fixture.slot);
    let authority = slot
        .authority()
        .ok_or("managed fixture did not bind lifecycle authority")?;
    let lifecycle = lifecycle_probe(&authority);
    let release = ManagedFixtureRelease::new(authority);
    let pause = lifecycle.pause_before_next_poll();
    assert!(pause.wait_until_paused(Instant::now() + OBSERVATION_BOUND));
    assert!(fixture.finish(Arc::clone(&registry)).is_err());
    let ticket = retention
        .only_ticket()
        .ok_or("fixture did not retain exactly one gated launch")?;

    assert!(wait_until(Instant::now() + OBSERVATION_BOUND, || {
        retention.snapshot().managed_running >= 2
    }));
    let blocked = lifecycle.snapshot();
    assert_eq!(blocked.terminate_calls, 1);
    assert_eq!(blocked.terminate_commands, 0);
    assert_eq!(blocked.native_reaps, 0);
    assert!(blocked.actual_exit.is_none());
    assert!(blocked.owns_unreaped_child);
    assert!(blocked.resources_present);
    let retained = retention.snapshot();
    assert_eq!(retained.retained, 1);
    assert_eq!(retained.unreaped, 1);
    assert!(retention.contains(ticket));
    assert!(plan_path.exists());
    assert_eq!(gated.adapter.destructive_calls(), (0, 0));

    drop(pause);
    assert!(wait_until(Instant::now() + REAP_BOUND, || {
        lifecycle.snapshot().actual_exit.is_some() && retention.snapshot().unreaped == 0
    }));
    let reaped_lifecycle = lifecycle.snapshot();
    assert_eq!(reaped_lifecycle.terminate_calls, 1);
    assert_eq!(reaped_lifecycle.terminate_commands, 1);
    assert_eq!(reaped_lifecycle.native_reaps, 1);
    assert!(!reaped_lifecycle.owns_unreaped_child);
    assert!(reaped_lifecycle.resources_present);
    let reaped_retention = retention.snapshot();
    assert_eq!(reaped_retention.retained, 1);
    assert_eq!(reaped_retention.unreaped, 0);
    assert!(retention.contains(ticket));
    assert!(plan_path.exists());
    assert_eq!(gated.adapter.destructive_calls(), (0, 0));

    release.disarm();
    drop(slot);
    assert!(wait_until(Instant::now() + OBSERVATION_BOUND, || {
        let snapshot = lifecycle.snapshot();
        snapshot.actor_finished && !snapshot.resources_present
    }));
    Ok(())
}

#[test]
fn retained_unbound_bind_fault_invokes_kill_before_natural_exit_and_only_once(
) -> Result<(), Box<dyn Error>> {
    let gated = gated_fixture("unbound")?;
    let plan_path = gated._control.path().join("launch-plan.json");
    let registry = LaunchRetentionRegistry::try_new()?;
    let retention = registry_probe(&registry);
    let adapter_trait: Arc<dyn GatedProviderAdapter> = gated.adapter.clone();
    let fixture = spawn_fixture(
        FixtureSpawnSpec {
            mode: FixtureIoMode::Piped,
            executable: "/bin/sh",
            args: vec!["-c".into(), "exec sleep 1".into()],
            seeded_prompt: None,
            grace_timeout: Duration::from_millis(50),
            stuck_timeout: Duration::from_millis(50),
            gate: Some(FixtureSpawnGate {
                plan: gated.plan,
                adapter: adapter_trait,
                enclosing: Instant::now() + Duration::from_secs(5),
            }),
            fault: Some(FixtureSpawnFault::BindDisconnected),
        },
        Arc::clone(&registry),
    );
    assert!(fixture.finish(Arc::clone(&registry)).is_err());
    let ticket = retention
        .only_ticket()
        .ok_or("fixture did not retain exactly one gated launch")?;

    let first_poll_kill_observed = wait_until(Instant::now() + FIRST_POLL_BOUND, || {
        retention.snapshot().unbound_kill_calls == 1
    });
    assert!(wait_until(Instant::now() + REAP_BOUND, || {
        retention.snapshot().unreaped == 0
    }));
    assert!(first_poll_kill_observed);
    let reaped = retention.snapshot();
    assert_eq!(reaped.unbound_kill_calls, 1);
    assert_eq!(reaped.retained, 1);
    assert_eq!(reaped.unreaped, 0);
    assert!(retention.contains(ticket));
    assert!(plan_path.exists());
    assert_eq!(gated.adapter.destructive_calls(), (0, 0));

    drop(registry);
    assert!(retention.snapshot().unbound_kill_calls <= 1);
    Ok(())
}
