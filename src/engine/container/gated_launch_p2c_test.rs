use std::collections::VecDeque;
use std::error::Error;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::Utc;
use sha2::{Digest, Sha256};

use super::backend::ContainerBackend;
use super::gated_launch::child_lifecycle_test_support::{
    lifecycle_probe, unbound_kind_and_started_at, CreateCliKind,
};
use super::gated_launch::retention_test_support::{
    drop_last_owner_on_worker, registry_probe, registry_thread_start_failure,
};
use super::gated_launch::{
    canonical_inspection_revision, observe_started_create, AbsenceObservation,
    CanonicalInspectionRevisionInput, ChildLifecycleState, DurableLaunchPlan, ExactInspection,
    GatedProviderAdapter, ImmutableImageId, InspectionObservationKind, LaunchIdentityError,
    LaunchRetentionInitError, LaunchRetentionReason, LaunchRetentionRegistry, NamePresence,
    ProviderCallDeadline, ProviderKind, ProviderLaunchInspection, ProviderLaunchKey, ProviderState,
    RetainedAgentLaunch, RetainedExecution, SanitizedProviderStateObservation, SpawnStageError,
    StartedCreateObservation, REGISTRY_SHUTDOWN_GRACE,
};
use super::options::{ContainerName, ImageRef, ResolvedContainerOptions};
use super::process::test_support::{
    spawn_fixture, FixtureIoMode, FixtureSpawnFault, FixtureSpawnGate, FixtureSpawnResult,
    FixtureSpawnSpec,
};
use crate::data::session::{AgentHandle, Session};
use crate::data::startup_gate::load_startup_gate;
use crate::engine::agent_runtime::execution::{
    AgentExitInfo, AgentInstance, AgentStats, StuckEvent,
};
use crate::engine::error::EngineError;

const SHELL: &str = "/bin/sh";
const MISSING_EXECUTABLE: &str = "/awman-p2c-fixture-does-not-exist";
const NAME: &str = "awman-altana-p2c-custody";
const TOKEN: &str = "6767676767676767676767676767676767676767676767676767676767676767";
const IMAGE: &str = "sha256:abababababababababababababababababababababababababababababababab";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProviderCall {
    ResolveImage,
    InspectAbsence,
    InspectLaunch,
    ClassifyCreateExit,
    Stop,
    Remove,
}

#[derive(Clone, Copy)]
enum LaunchReply {
    Matching,
    Absent,
    Unavailable,
}

struct ScriptedAdapter {
    calls: Mutex<Vec<ProviderCall>>,
    replies: Mutex<VecDeque<LaunchReply>>,
}

impl ScriptedAdapter {
    fn new(replies: impl IntoIterator<Item = LaunchReply>) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            replies: Mutex::new(replies.into_iter().collect()),
        }
    }

    fn calls(&self) -> Vec<ProviderCall> {
        self.calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn record(&self, call: ProviderCall) {
        self.calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(call);
    }

    fn absence(&self, name: &ContainerName) -> AbsenceObservation {
        let revision = canonical_inspection_revision(CanonicalInspectionRevisionInput {
            kind: InspectionObservationKind::Absent,
            provider: ProviderKind::AppleContainers,
            exact_name: name,
            runtime_id: None,
            token_digest: None,
            immutable_image_id: None,
            created_at: None,
            state: SanitizedProviderStateObservation::Absent,
        });
        AbsenceObservation {
            provider: ProviderKind::AppleContainers,
            exact_name: name.clone(),
            checked_at: Utc::now(),
            revision,
        }
    }

    fn matching(&self, key: &ProviderLaunchKey) -> ProviderLaunchInspection {
        let created_at = std::cmp::max(key.created_not_before, Utc::now());
        let state = ProviderState::Apple(super::gated_launch::AppleProviderState::Running);
        let revision = canonical_inspection_revision(CanonicalInspectionRevisionInput {
            kind: InspectionObservationKind::Matching,
            provider: key.provider,
            exact_name: &key.container_name,
            runtime_id: Some("p2c-local-fixture-runtime"),
            token_digest: Some(key.token_digest),
            immutable_image_id: Some(&key.immutable_image_id),
            created_at: Some(created_at),
            state: SanitizedProviderStateObservation::Known(state.clone()),
        });
        ProviderLaunchInspection {
            provider: key.provider,
            runtime_id: "p2c-local-fixture-runtime".into(),
            exact_name: key.container_name.clone(),
            token_digest: key.token_digest,
            immutable_image_id: key.immutable_image_id.clone(),
            created_at,
            state,
            revision,
        }
    }

    fn destructive_calls(&self) -> usize {
        self.calls()
            .into_iter()
            .filter(|call| matches!(call, ProviderCall::Stop | ProviderCall::Remove))
            .count()
    }

    fn classification_calls(&self) -> usize {
        self.calls()
            .into_iter()
            .filter(|call| *call == ProviderCall::ClassifyCreateExit)
            .count()
    }
}

impl GatedProviderAdapter for ScriptedAdapter {
    fn provider(&self) -> ProviderKind {
        ProviderKind::AppleContainers
    }

    fn resolve_image_identity(
        &self,
        _image: &ImageRef,
        _deadline: ProviderCallDeadline,
    ) -> Result<ImmutableImageId, LaunchIdentityError> {
        self.record(ProviderCall::ResolveImage);
        ImmutableImageId::new(IMAGE)
    }

    fn inspect_name_absence(
        &self,
        name: &ContainerName,
        _deadline: ProviderCallDeadline,
    ) -> Result<AbsenceObservation, NamePresence> {
        self.record(ProviderCall::InspectAbsence);
        Ok(self.absence(name))
    }

    fn inspect_launch(
        &self,
        key: &ProviderLaunchKey,
        _deadline: ProviderCallDeadline,
    ) -> ExactInspection {
        self.record(ProviderCall::InspectLaunch);
        match self
            .replies
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pop_front()
            .unwrap_or(LaunchReply::Unavailable)
        {
            LaunchReply::Matching => ExactInspection::Matching(self.matching(key)),
            LaunchReply::Absent => ExactInspection::Absent(self.absence(&key.container_name)),
            LaunchReply::Unavailable => ExactInspection::Unavailable,
        }
    }

    fn classify_create_exit(
        &self,
        _exit: &super::gated_launch::CreateCliExitEvidence,
    ) -> super::gated_launch::CreateCliExitClass {
        self.record(ProviderCall::ClassifyCreateExit);
        super::gated_launch::CreateCliExitClass::Other
    }

    fn stop_inspected(
        &self,
        _inspection: &ProviderLaunchInspection,
        _deadline: ProviderCallDeadline,
    ) -> Result<(), EngineError> {
        self.record(ProviderCall::Stop);
        Err(EngineError::Container(
            "Packet 2 fixture forbids provider stop".into(),
        ))
    }

    fn remove_inspected(
        &self,
        _inspection: &ProviderLaunchInspection,
        _deadline: ProviderCallDeadline,
    ) -> Result<(), EngineError> {
        self.record(ProviderCall::Remove);
        Err(EngineError::Container(
            "Packet 2 fixture forbids provider remove".into(),
        ))
    }
}

#[cfg(unix)]
struct ControlFixture {
    _temp: tempfile::TempDir,
    parent: PathBuf,
}

#[cfg(unix)]
impl ControlFixture {
    fn new() -> Result<Self, Box<dyn Error>> {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir()?;
        let parent = temp.path().join("control-parent");
        let guest = parent.join("guest-control");
        std::fs::create_dir(&parent)?;
        std::fs::create_dir(&guest)?;
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700))?;
        std::fs::set_permissions(&guest, std::fs::Permissions::from_mode(0o700))?;

        let manifest = b"{\"version\":1,\"entries\":[]}\n";
        let manifest_digest = lower_hex(&Sha256::digest(manifest));
        let request = format!(
            "{{\"version\":1,\"bindings\":[{{\"id\":\"workspace\",\"workspace_path\":\"/workspace\",\"manifest_id\":\"{manifest_digest}\",\"manifest_file\":\"workspace.manifest.json\",\"access\":\"read-only\"}}]}}\n"
        );
        let intent = format!(
            "{{\"version\":1,\"container_name\":\"{NAME}\",\"launch_token\":\"{TOKEN}\"}}\n"
        );
        write_private(&parent.join("workspace.manifest.json"), manifest)?;
        write_private(&parent.join("request.json"), request.as_bytes())?;
        write_private(&parent.join("launch-intent.json"), intent.as_bytes())?;
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
            &ImageRef::new("awman-p2c-fixture:latest"),
            identity,
            Arc::clone(controls),
            Instant::now() + Duration::from_secs(4),
        )?)
    }
}

#[cfg(unix)]
struct LocalFixture {
    _temp: tempfile::TempDir,
    script: PathBuf,
    marker: PathBuf,
    release: PathBuf,
    tag: String,
}

#[cfg(unix)]
impl LocalFixture {
    fn new(tag: impl Into<String>) -> Result<Self, Box<dyn Error>> {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir()?;
        let script = temp.path().join("trusted-child.sh");
        let marker = temp.path().join("started.marker");
        let release = temp.path().join("release.marker");
        let bytes = b"#!/bin/sh\nset -eu\nmarker=$1\nrelease=$2\ncode=$3\ntag=$4\noutput=$5\nprintf '%s\\n' \"$tag\" >> \"$marker\"\nif [ \"$output\" != \"-\" ]; then printf '%s\\n' \"$output\"; fi\nremaining=500\nwhile [ ! -f \"$release\" ] && [ \"$remaining\" -gt 0 ]; do\n  /bin/sleep 0.02\n  remaining=$((remaining - 1))\ndone\nexit \"$code\"\n";
        std::fs::write(&script, bytes)?;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))?;
        Ok(Self {
            _temp: temp,
            script,
            marker,
            release,
            tag: tag.into(),
        })
    }

    fn args(&self, code: i32, output: &str) -> Vec<String> {
        vec![
            self.script.to_string_lossy().into_owned(),
            self.marker.to_string_lossy().into_owned(),
            self.release.to_string_lossy().into_owned(),
            code.to_string(),
            self.tag.clone(),
            output.into(),
        ]
    }

    fn release(&self) -> Result<(), Box<dyn Error>> {
        write_private(&self.release, b"release\n")?;
        Ok(())
    }

    fn starts(&self) -> Result<usize, Box<dyn Error>> {
        match std::fs::read_to_string(&self.marker) {
            Ok(text) => Ok(text.lines().count()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(error) => Err(error.into()),
        }
    }
}

#[cfg(unix)]
impl Drop for LocalFixture {
    fn drop(&mut self) {
        if let Err(error) = write_private(&self.release, b"drop-release\n") {
            eprintln!("Packet 2 fixture release failed during Drop: {error}");
        }
    }
}

#[cfg(unix)]
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

#[cfg(unix)]
struct Invocation {
    local: LocalFixture,
    _controls: ControlFixture,
    plan: DurableLaunchPlan,
    adapter: Arc<ScriptedAdapter>,
    registry: Arc<LaunchRetentionRegistry>,
    spawned: FixtureSpawnResult,
}

#[cfg(unix)]
struct InvokeOptions {
    mode: FixtureIoMode,
    code: i32,
    output: String,
    grace_timeout: Duration,
    executable: &'static str,
    fault: Option<FixtureSpawnFault>,
    replies: Vec<LaunchReply>,
}

#[cfg(unix)]
fn invoke(options: InvokeOptions) -> Result<Invocation, Box<dyn Error>> {
    let local = LocalFixture::new(format!("p2c-{}", uuid::Uuid::new_v4()))?;
    let controls = ControlFixture::new()?;
    let adapter = Arc::new(ScriptedAdapter::new(options.replies));
    let plan = controls.plan(adapter.as_ref())?;
    let registry = LaunchRetentionRegistry::try_new()
        .map_err(|_| "launch retention registry failed to start")?;
    let gate_adapter: Arc<dyn GatedProviderAdapter> = adapter.clone();
    let spawned = spawn_fixture(
        FixtureSpawnSpec {
            mode: options.mode,
            executable: options.executable,
            args: local.args(options.code, &options.output),
            seeded_prompt: None,
            grace_timeout: options.grace_timeout,
            stuck_timeout: Duration::from_secs(2),
            gate: Some(FixtureSpawnGate {
                plan: plan.clone(),
                adapter: gate_adapter,
                enclosing: Instant::now() + Duration::from_secs(4),
            }),
            fault: options.fault,
        },
        Arc::clone(&registry),
    );
    Ok(Invocation {
        local,
        _controls: controls,
        plan,
        adapter,
        registry,
        spawned,
    })
}

#[cfg(unix)]
fn mode(index: usize) -> FixtureIoMode {
    match index {
        0 => FixtureIoMode::Pty { cols: 80, rows: 24 },
        1 => FixtureIoMode::Piped,
        _ => FixtureIoMode::PersistentPiped,
    }
}

fn kind(index: usize) -> CreateCliKind {
    match index {
        0 => CreateCliKind::Pty,
        1 => CreateCliKind::Piped,
        _ => CreateCliKind::PersistentPiped,
    }
}

#[cfg(unix)]
fn after_start(error: SpawnStageError) -> Result<(EngineError, RetainedExecution), Box<dyn Error>> {
    match error {
        SpawnStageError::AfterCliStart { source, owned } => Ok((source, owned)),
        SpawnStageError::BeforeCliStart(_) => Err("expected child custody after CLI start".into()),
    }
}

fn wait_until(deadline: Instant, mut predicate: impl FnMut() -> bool) -> bool {
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    predicate()
}

async fn output_contains(
    receiver: &mut tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    expected: &str,
) -> bool {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut bytes = Vec::new();
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remaining, receiver.recv()).await {
            Ok(Some(chunk)) => {
                bytes.extend(chunk);
                if String::from_utf8_lossy(&bytes).contains(expected) {
                    return true;
                }
            }
            Ok(None) | Err(_) => break,
        }
    }
    false
}

fn assert_same_exit(left: &AgentExitInfo, right: &AgentExitInfo) {
    assert_eq!(left, right);
    assert_eq!(left.started_at, right.started_at);
    assert_eq!(left.ended_at, right.ended_at);
}

#[derive(Default)]
struct LegacyBackend {
    build_calls: AtomicUsize,
}

impl ContainerBackend for LegacyBackend {
    fn build(
        &self,
        _options: ResolvedContainerOptions,
    ) -> Result<Box<dyn AgentInstance>, EngineError> {
        self.build_calls.fetch_add(1, Ordering::SeqCst);
        Err(EngineError::Container("legacy build invoked".into()))
    }

    fn list_running(&self, _session: &Session) -> Result<Vec<AgentHandle>, EngineError> {
        Ok(Vec::new())
    }

    fn stats(&self, _handle: &AgentHandle) -> Result<AgentStats, EngineError> {
        Err(EngineError::Container("unused fixture stats".into()))
    }

    fn attach(&self, _handle: &AgentHandle) -> Result<Box<dyn AgentInstance>, EngineError> {
        Err(EngineError::Container("unused fixture attach".into()))
    }

    fn list_running_with_name_prefix(
        &self,
        _prefix: &str,
    ) -> Result<Vec<AgentHandle>, EngineError> {
        Ok(Vec::new())
    }

    fn name(&self) -> &'static str {
        "p2c-legacy-fixture"
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn all_three_real_spawn_functions_bind_once_and_publish_one_actual_exit(
) -> Result<(), Box<dyn Error>> {
    for index in 0..3 {
        let Invocation {
            local,
            _controls,
            plan: _,
            adapter,
            registry: _,
            spawned,
        } = invoke(InvokeOptions {
            mode: mode(index),
            code: 0,
            output: format!("packet2-mode-{index}"),
            grace_timeout: Duration::from_secs(2),
            executable: SHELL,
            fault: None,
            replies: vec![LaunchReply::Matching],
        })?;
        let FixtureSpawnResult {
            result,
            slot,
            mut stdout,
            stderr: _,
            ..
        } = spawned;
        let mut execution = result.map_err(|_| "trusted fixture spawn failed")?;
        let authority = slot.authority().ok_or("spawn returned without lifecycle")?;
        let probe = lifecycle_probe(&authority);
        assert_eq!(authority.state()?, ChildLifecycleState::Running);
        assert!(output_contains(&mut stdout, &format!("packet2-mode-{index}")).await);
        assert_eq!(local.starts()?, 1);

        local.release()?;
        let exit = execution.wait().await?;
        let repeated = authority.wait_actual()?;
        assert_same_exit(&exit, &repeated);
        let snapshot = probe.snapshot();
        assert_eq!(snapshot.native_reaps, 1);
        assert_eq!(snapshot.actual_exit.as_ref(), Some(&exit));
        assert!(!snapshot.owns_unreaped_child);
        assert_eq!(adapter.calls()[0], ProviderCall::ResolveImage);
        assert_eq!(
            adapter
                .calls()
                .into_iter()
                .filter(|call| *call == ProviderCall::InspectAbsence)
                .count(),
            2
        );
        assert_eq!(adapter.destructive_calls(), 0);
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn actor_start_failure_precedes_barrier_and_executes_no_fixture() -> Result<(), Box<dyn Error>> {
    let Invocation {
        local,
        _controls,
        plan: _,
        adapter,
        registry: _,
        spawned,
    } = invoke(InvokeOptions {
        mode: FixtureIoMode::Piped,
        code: 0,
        output: "must-not-run".into(),
        grace_timeout: Duration::from_secs(2),
        executable: SHELL,
        fault: Some(FixtureSpawnFault::ActorThreadStart),
        replies: vec![],
    })?;
    match spawned.result {
        Err(SpawnStageError::BeforeCliStart(EngineError::Container(message))) => {
            assert_eq!(message, "create CLI actor unavailable");
        }
        _ => return Err("actor-start fault did not fail before CLI start".into()),
    }
    assert!(spawned.slot.authority().is_none());
    assert_eq!(local.starts()?, 0);
    assert_eq!(
        adapter
            .calls()
            .into_iter()
            .filter(|call| *call == ProviderCall::InspectAbsence)
            .count(),
        1
    );
    assert_eq!(adapter.destructive_calls(), 0);
    Ok(())
}

#[cfg(unix)]
#[test]
fn bind_failures_return_each_exact_raw_child_variant_for_synchronous_retention(
) -> Result<(), Box<dyn Error>> {
    for index in 0..3 {
        for disconnected in [true, false] {
            let before = Utc::now();
            let Invocation {
                local,
                _controls,
                plan,
                adapter,
                registry,
                spawned,
            } = invoke(InvokeOptions {
                mode: mode(index),
                code: 0,
                output: "bind-fault".into(),
                grace_timeout: Duration::from_secs(2),
                executable: SHELL,
                fault: Some(if disconnected {
                    FixtureSpawnFault::BindDisconnected
                } else {
                    FixtureSpawnFault::BindFull
                }),
                replies: vec![],
            })?;
            let after = Utc::now();
            let (source, owned) = after_start(
                spawned
                    .result
                    .err()
                    .ok_or("bind fault unexpectedly succeeded")?,
            )?;
            assert!(matches!(
                source,
                EngineError::Container(ref message) if message == "create CLI state unknown"
            ));
            let unbound = match &owned {
                RetainedExecution::OwnedUnbound { child, resources } => {
                    let _ = resources;
                    child
                }
                RetainedExecution::Unbound(_) => {
                    return Err("bind fault returned child without resource custody".into());
                }
                RetainedExecution::Managed { .. } => {
                    return Err("bind fault returned managed custody".into());
                }
            };
            let (actual_kind, started_at) = unbound_kind_and_started_at(unbound);
            assert_eq!(actual_kind, kind(index));
            assert!(started_at >= before && started_at <= after);
            assert!(spawned.slot.authority().is_none());

            let probe = registry_probe(&registry);
            let ticket = registry.retain(RetainedAgentLaunch {
                plan,
                execution: Some(owned),
                last_inspection: None,
                reason: LaunchRetentionReason::ChildStateUnknown,
            });
            assert!(probe.contains(ticket));
            local.release()?;
            assert!(wait_until(
                Instant::now() + Duration::from_secs(3),
                || probe.snapshot().unreaped == 0
            ));
            assert!(local.starts()? <= 1);
            assert_eq!(adapter.destructive_calls(), 0);
        }
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn post_bind_bridge_fault_keeps_managed_authority_for_all_three_representations(
) -> Result<(), Box<dyn Error>> {
    for index in 0..3 {
        let Invocation {
            local,
            _controls,
            plan,
            adapter,
            registry,
            spawned,
        } = invoke(InvokeOptions {
            mode: mode(index),
            code: 0,
            output: "post-bind-fault".into(),
            grace_timeout: Duration::from_secs(2),
            executable: SHELL,
            fault: Some(FixtureSpawnFault::AfterBindBeforeBridge),
            replies: vec![LaunchReply::Matching],
        })?;
        let (source, owned) = after_start(
            spawned
                .result
                .err()
                .ok_or("post-bind fault unexpectedly succeeded")?,
        )?;
        assert!(matches!(
            source,
            EngineError::Container(ref message) if message == "create CLI bridge setup failed"
        ));
        let authority = match &owned {
            RetainedExecution::Managed {
                execution: None,
                lifecycle,
            } => lifecycle.clone(),
            _ => return Err("post-bind fault did not return managed pre-execution custody".into()),
        };
        let slot_authority = spawned
            .slot
            .authority()
            .ok_or("managed fault did not bind the shared slot")?;
        assert_eq!(authority.state()?, slot_authority.state()?);
        let lifecycle = lifecycle_probe(&authority);
        let retained = registry_probe(&registry);
        let ticket = registry.retain(RetainedAgentLaunch {
            plan,
            execution: Some(owned),
            last_inspection: None,
            reason: LaunchRetentionReason::SpawnResultUnknown,
        });
        assert!(retained.contains(ticket));
        local.release()?;
        let exit = authority.wait_actual()?;
        let snapshot = lifecycle.snapshot();
        assert_eq!(snapshot.actual_exit.as_ref(), Some(&exit));
        assert_eq!(snapshot.native_reaps, 1);
        assert!(!snapshot.owns_unreaped_child);
        assert!(local.starts()? <= 1);
        assert_eq!(adapter.destructive_calls(), 0);
    }
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_start_absence_and_nonzero_matching_exit_remain_retained_without_retry(
) -> Result<(), Box<dyn Error>> {
    for (code, second_reply) in [(0, LaunchReply::Absent), (23, LaunchReply::Matching)] {
        let Invocation {
            local,
            _controls,
            plan,
            adapter,
            registry: _,
            spawned,
        } = invoke(InvokeOptions {
            mode: FixtureIoMode::Piped,
            code,
            output: format!("post-start-{code}"),
            grace_timeout: Duration::from_secs(2),
            executable: SHELL,
            fault: None,
            replies: vec![LaunchReply::Matching, second_reply],
        })?;
        let mut execution = spawned.result.map_err(|_| "fixture spawn failed")?;
        let authority = spawned.slot.authority().ok_or("missing lifecycle")?;
        local.release()?;
        let exit = execution.wait().await?;
        assert_eq!(exit.exit_code, code);
        assert!(matches!(
            observe_started_create(
                &plan,
                adapter.as_ref(),
                &authority,
                Instant::now() + Duration::from_secs(2),
            ),
            StartedCreateObservation::Retained(LaunchRetentionReason::SpawnResultUnknown)
        ));
        assert_eq!(local.starts()?, 1);
        assert_eq!(lifecycle_probe(&authority).snapshot().native_reaps, 1);
        assert_eq!(adapter.classification_calls(), 0);
        assert_eq!(adapter.destructive_calls(), 0);
    }
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn matching_observation_is_only_tuple_convergence_while_child_is_live(
) -> Result<(), Box<dyn Error>> {
    let Invocation {
        local,
        _controls,
        plan,
        adapter,
        registry: _,
        spawned,
    } = invoke(InvokeOptions {
        mode: FixtureIoMode::Pty { cols: 80, rows: 24 },
        code: 0,
        output: "matching-live".into(),
        grace_timeout: Duration::from_secs(2),
        executable: SHELL,
        fault: None,
        replies: vec![LaunchReply::Matching, LaunchReply::Matching],
    })?;
    let mut execution = spawned.result.map_err(|_| "fixture spawn failed")?;
    let authority = spawned.slot.authority().ok_or("missing lifecycle")?;
    match observe_started_create(
        &plan,
        adapter.as_ref(),
        &authority,
        Instant::now() + Duration::from_secs(2),
    ) {
        StartedCreateObservation::Matching(inspection) => {
            assert!(inspection.exact_name == plan.key.container_name);
        }
        StartedCreateObservation::Retained(_) => {
            return Err("live matching tuple did not converge".into());
        }
    }
    assert_eq!(authority.state()?, ChildLifecycleState::Running);
    assert_eq!(adapter.destructive_calls(), 0);
    local.release()?;
    let _ = execution.wait().await?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn genuine_missing_executable_is_the_only_fixture_path_with_no_child_custody(
) -> Result<(), Box<dyn Error>> {
    let Invocation {
        local,
        _controls,
        plan: _,
        adapter,
        registry: _,
        spawned,
    } = invoke(InvokeOptions {
        mode: FixtureIoMode::Piped,
        code: 0,
        output: "missing".into(),
        grace_timeout: Duration::from_secs(2),
        executable: MISSING_EXECUTABLE,
        fault: None,
        replies: vec![],
    })?;
    match spawned.result {
        Err(SpawnStageError::BeforeCliStart(EngineError::ContainerRuntimeUnavailable {
            binary,
        })) => assert_eq!(binary, MISSING_EXECUTABLE),
        _ => {
            return Err(
                "missing executable did not return its binary-specific no-child error".into(),
            )
        }
    }
    assert!(spawned.slot.authority().is_none());
    assert_eq!(local.starts()?, 0);
    assert_eq!(
        adapter
            .calls()
            .into_iter()
            .filter(|call| *call == ProviderCall::InspectAbsence)
            .count(),
        2
    );
    assert_eq!(adapter.destructive_calls(), 0);
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn cancel_handle_bounded_queries_and_legacy_wait_share_one_actual_reap(
) -> Result<(), Box<dyn Error>> {
    let Invocation {
        local,
        _controls,
        plan: _,
        adapter,
        registry: _,
        spawned,
    } = invoke(InvokeOptions {
        mode: FixtureIoMode::PersistentPiped,
        code: 0,
        output: "cancel-handle".into(),
        grace_timeout: Duration::from_secs(2),
        executable: SHELL,
        fault: None,
        replies: vec![LaunchReply::Matching],
    })?;
    let mut execution = spawned.result.map_err(|_| "fixture spawn failed")?;
    let cancel = execution.cancel_handle().ok_or("missing cancel handle")?;
    let authority = spawned.slot.authority().ok_or("missing lifecycle")?;
    let probe = lifecycle_probe(&authority);
    let legacy_authority = authority.clone();
    let bounded_authority = authority.clone();
    let legacy = tokio::task::spawn_blocking(move || legacy_authority.wait_actual());
    let bounded =
        tokio::task::spawn_blocking(move || -> Result<Option<AgentExitInfo>, EngineError> {
            bounded_authority.wait_actual_until(Instant::now() + Duration::from_secs(3))
        });
    cancel.cancel()?;
    let execution_exit = tokio::time::timeout(Duration::from_secs(3), execution.wait()).await??;
    local.release()?;
    let legacy_exit = legacy.await??;
    let bounded_exit = bounded.await??.ok_or("bounded waiter missed actual exit")?;
    assert_same_exit(&execution_exit, &legacy_exit);
    assert_same_exit(&execution_exit, &bounded_exit);
    assert_eq!(probe.snapshot().native_reaps, 1);
    assert_eq!(adapter.destructive_calls(), 0);
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_execution_cancel_and_grace_expiry_use_the_bound_actor(
) -> Result<(), Box<dyn Error>> {
    {
        let Invocation {
            local,
            _controls,
            plan: _,
            adapter,
            registry: _,
            spawned,
        } = invoke(InvokeOptions {
            mode: FixtureIoMode::Piped,
            code: 0,
            output: "explicit-cancel".into(),
            grace_timeout: Duration::from_secs(2),
            executable: SHELL,
            fault: None,
            replies: vec![LaunchReply::Matching],
        })?;
        let mut execution = spawned.result.map_err(|_| "fixture spawn failed")?;
        let authority = spawned.slot.authority().ok_or("missing lifecycle")?;
        let probe = lifecycle_probe(&authority);
        execution.cancel()?;
        let exit = tokio::time::timeout(Duration::from_secs(3), execution.wait()).await??;
        local.release()?;
        assert_same_exit(&exit, &authority.wait_actual()?);
        assert_eq!(probe.snapshot().native_reaps, 1);
        assert_eq!(adapter.destructive_calls(), 0);
    }
    {
        let Invocation {
            local,
            _controls,
            plan: _,
            adapter,
            registry: _,
            spawned,
        } = invoke(InvokeOptions {
            mode: FixtureIoMode::Piped,
            code: 0,
            output: "-".into(),
            grace_timeout: Duration::from_millis(80),
            executable: SHELL,
            fault: None,
            replies: vec![LaunchReply::Matching],
        })?;
        let mut execution = spawned.result.map_err(|_| "fixture spawn failed")?;
        let authority = spawned.slot.authority().ok_or("missing lifecycle")?;
        let probe = lifecycle_probe(&authority);
        let mut stuck = execution.subscribe_stuck();
        let exit = tokio::time::timeout(Duration::from_secs(3), execution.wait()).await??;
        let event = tokio::time::timeout(Duration::from_secs(2), stuck.recv()).await??;
        assert_eq!(event, StuckEvent::StartupGraceExpired);
        assert_same_exit(&exit, &authority.wait_actual()?);
        assert_eq!(probe.snapshot().native_reaps, 1);
        assert_eq!(adapter.destructive_calls(), 0);
        local.release()?;
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn retention_is_synchronous_recovers_poison_and_remains_responsive_while_actor_is_paused(
) -> Result<(), Box<dyn Error>> {
    let Invocation {
        local,
        _controls,
        plan,
        adapter,
        registry,
        spawned,
    } = invoke(InvokeOptions {
        mode: FixtureIoMode::Piped,
        code: 0,
        output: "paused-actor".into(),
        grace_timeout: Duration::from_secs(2),
        executable: SHELL,
        fault: Some(FixtureSpawnFault::AfterBindBeforeBridge),
        replies: vec![LaunchReply::Matching],
    })?;
    let (_, owned) = after_start(
        spawned
            .result
            .err()
            .ok_or("post-bind fixture unexpectedly succeeded")?,
    )?;
    let authority = match &owned {
        RetainedExecution::Managed { lifecycle, .. } => lifecycle.clone(),
        RetainedExecution::Unbound(_) | RetainedExecution::OwnedUnbound { .. } => {
            return Err("expected managed lifecycle".into());
        }
    };
    let lifecycle = lifecycle_probe(&authority);
    let pause = lifecycle.pause_before_next_poll();
    assert!(pause.wait_until_paused(Instant::now() + Duration::from_secs(2)));
    let retained = registry_probe(&registry);
    let started = Instant::now();
    let first = registry.retain(RetainedAgentLaunch {
        plan,
        execution: Some(owned),
        last_inspection: None,
        reason: LaunchRetentionReason::SpawnResultUnknown,
    });
    assert!(started.elapsed() < Duration::from_millis(500));
    assert!(retained.contains(first));
    assert_eq!(authority.state()?, ChildLifecycleState::Running);

    retained.poison_entries();
    let second_controls = ControlFixture::new()?;
    let second_adapter = ScriptedAdapter::new([]);
    let second_plan = second_controls.plan(&second_adapter)?;
    let second = registry.retain(RetainedAgentLaunch {
        plan: second_plan,
        execution: None,
        last_inspection: None,
        reason: LaunchRetentionReason::InspectionUnavailable,
    });
    assert!(retained.contains(first));
    assert!(retained.contains(second));
    let snapshot = retained.snapshot();
    assert!(snapshot.supervisor_failed);
    assert!(snapshot.retained >= 2);
    assert_eq!(adapter.destructive_calls(), 0);

    local.release()?;
    drop(pause);
    assert!(wait_until(
        Instant::now() + Duration::from_secs(3),
        || lifecycle.snapshot().native_reaps == 1
    ));
    Ok(())
}

#[cfg(unix)]
#[test]
fn registry_start_failure_exists_before_any_fixture_spawn() -> Result<(), Box<dyn Error>> {
    let local = LocalFixture::new("registry-start-failure")?;
    let error = match registry_thread_start_failure() {
        Ok(_) => return Err("injected registry thread failure unexpectedly succeeded".into()),
        Err(error) => error,
    };
    assert_eq!(error, LaunchRetentionInitError::SupervisorThreadUnavailable);
    assert_eq!(local.starts()?, 0);
    Ok(())
}

#[cfg(unix)]
#[test]
fn default_backend_method_rejects_orchestrated_input_before_legacy_build(
) -> Result<(), Box<dyn Error>> {
    let controls = ControlFixture::new()?;
    let spec = load_startup_gate(&controls.parent, Duration::from_secs(30))?;
    assert!(spec.control.orchestrated_parts().is_some());
    let orchestrated = ResolvedContainerOptions {
        startup_gate: Some(Box::new(spec)),
        ..Default::default()
    };
    let registry = LaunchRetentionRegistry::try_new()
        .map_err(|_| "default-backend registry failed to start")?;
    let backend = LegacyBackend::default();
    let error = match backend.build_with_launch_retention(orchestrated, Some(registry)) {
        Ok(_) => return Err("legacy backend accepted orchestrated launch".into()),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        EngineError::Config(ref message)
            if message == "orchestrated launch retention is unavailable"
    ));
    assert_eq!(backend.build_calls.load(Ordering::SeqCst), 0);

    let legacy_error =
        match backend.build_with_launch_retention(ResolvedContainerOptions::default(), None) {
            Ok(_) => return Err("legacy fixture build unexpectedly succeeded".into()),
            Err(error) => error,
        };
    assert!(matches!(
        legacy_error,
        EngineError::Container(ref message) if message == "legacy build invoked"
    ));
    assert_eq!(backend.build_calls.load(Ordering::SeqCst), 1);
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn last_owner_drop_detaches_managed_custody_without_provider_actions(
) -> Result<(), Box<dyn Error>> {
    let Invocation {
        local,
        _controls,
        plan,
        adapter,
        registry,
        spawned,
    } = invoke(InvokeOptions {
        mode: FixtureIoMode::Piped,
        code: 0,
        output: "managed-drop".into(),
        grace_timeout: Duration::from_secs(2),
        executable: SHELL,
        fault: None,
        replies: vec![LaunchReply::Matching],
    })?;
    let execution = spawned.result.map_err(|_| "fixture spawn failed")?;
    let authority = spawned.slot.authority().ok_or("missing lifecycle")?;
    let lifecycle = lifecycle_probe(&authority);
    let pause = lifecycle.pause_before_next_poll();
    assert!(pause.wait_until_paused(Instant::now() + Duration::from_secs(2)));
    let retained = registry_probe(&registry);
    let ticket = registry.retain(RetainedAgentLaunch {
        plan,
        execution: Some(RetainedExecution::Managed {
            execution: Some(execution),
            lifecycle: authority,
        }),
        last_inspection: None,
        reason: LaunchRetentionReason::SpawnResultUnknown,
    });
    assert!(retained.contains(ticket));
    let before_calls = adapter.calls();
    let weak = Arc::downgrade(&registry);
    let started = Instant::now();
    drop(registry);
    assert!(started.elapsed() <= REGISTRY_SHUTDOWN_GRACE + Duration::from_secs(1));
    assert!(weak.upgrade().is_none());
    let snapshot = retained.snapshot();
    assert!(snapshot.shutdown_requested);
    assert!(snapshot.worker_detached);
    assert_eq!(snapshot.retained, 1);
    assert!(snapshot.unreaped >= 1);
    assert!(!snapshot.worker_finished);
    assert_eq!(adapter.calls(), before_calls);

    local.release()?;
    drop(pause);
    assert!(wait_until(Instant::now() + Duration::from_secs(3), || {
        retained.snapshot().unreaped == 0
    }));
    let reaped = retained.snapshot();
    assert_eq!(reaped.retained, 1);
    assert!(!reaped.worker_finished);
    assert_eq!(adapter.destructive_calls(), 0);
    Ok(())
}

#[cfg(unix)]
#[test]
fn last_owner_drop_bounds_unbound_raw_custody_and_never_self_joins() -> Result<(), Box<dyn Error>> {
    let Invocation {
        local,
        _controls,
        plan,
        adapter,
        registry,
        spawned,
    } = invoke(InvokeOptions {
        mode: FixtureIoMode::PersistentPiped,
        code: 0,
        output: "unbound-drop".into(),
        grace_timeout: Duration::from_secs(2),
        executable: SHELL,
        fault: Some(FixtureSpawnFault::BindFull),
        replies: vec![],
    })?;
    let (_, owned) = after_start(
        spawned
            .result
            .err()
            .ok_or("bind-full fixture unexpectedly succeeded")?,
    )?;
    assert!(matches!(&owned, RetainedExecution::OwnedUnbound { .. }));
    let retained = registry_probe(&registry);
    let ticket = registry.retain(RetainedAgentLaunch {
        plan,
        execution: Some(owned),
        last_inspection: None,
        reason: LaunchRetentionReason::ChildStateUnknown,
    });
    assert!(retained.contains(ticket));
    let before_calls = adapter.calls();
    let weak = Arc::downgrade(&registry);
    let started = Instant::now();
    drop(registry);
    assert!(started.elapsed() <= REGISTRY_SHUTDOWN_GRACE + Duration::from_secs(1));
    assert!(weak.upgrade().is_none());
    let shutdown = retained.snapshot();
    assert!(shutdown.shutdown_requested);
    assert!(shutdown.worker_detached);
    assert_eq!(shutdown.retained, 1);
    assert!(shutdown.unreaped <= 1);
    assert!(!shutdown.worker_finished);
    assert_eq!(adapter.calls(), before_calls);
    local.release()?;
    assert!(wait_until(Instant::now() + Duration::from_secs(3), || {
        retained.snapshot().unreaped == 0
    }));
    let reaped = retained.snapshot();
    assert_eq!(reaped.retained, 1);
    assert!(!reaped.worker_finished);
    assert_eq!(adapter.destructive_calls(), 0);

    let self_drop_registry =
        LaunchRetentionRegistry::try_new().map_err(|_| "self-drop registry failed to start")?;
    let started = Instant::now();
    let self_drop = drop_last_owner_on_worker(
        self_drop_registry,
        Instant::now() + REGISTRY_SHUTDOWN_GRACE + Duration::from_secs(1),
    )?;
    assert!(started.elapsed() <= REGISTRY_SHUTDOWN_GRACE + Duration::from_secs(1));
    assert!(self_drop.snapshot().shutdown_requested);
    Ok(())
}
