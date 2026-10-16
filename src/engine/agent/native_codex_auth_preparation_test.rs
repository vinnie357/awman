//! First auth packet. The missing production APIs are intentional RED boundaries.
//! Only external Apple inspection/CLI, pipe faults and observation pauses are replaced.
//! This file supplies no preparation, retirement, retention or authentication authority.

use std::collections::BTreeMap;
use std::error::Error;
use std::fs::{self, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::agent_matrix::NativePrintRequest;
use super::native_codex_preparation::{AuthRetirement, AuthStartError, PreparationError};
use super::AgentEngine;
use crate::data::config::env::EnvSnapshot;
use crate::data::fs::auth_paths::AuthPathResolver;
use crate::data::message::{UserMessage, UserMessageSink};
use crate::data::session::{AgentName, Session, SessionOpenOptions};
use crate::engine::agent_runtime::{AgentFrontend, AgentIo, AgentProgress, AgentStatus};
use crate::engine::container::gated_launch::*;
use crate::engine::container::options::{ContainerName, ImageRef};
use crate::engine::container::orchestrated_apple::{
    OrchestratedAppleTestBoundary, TestCancellationPoint, TestOwnershipObservation,
};
use crate::engine::container::ContainerRuntime;
use crate::engine::error::EngineError;
use crate::engine::overlay::OverlayEngine;
use chrono::Utc;
use serde_json::{json, Value};

pub(crate) type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;
pub(crate) const QUESTION: &str = "MODEL_QUESTION_MUST_NOT_REACH_AUTH";
pub(crate) const URL: &str = "https://device.invalid/SYNTHETIC_DEVICE_CHALLENGE";
const NONCE: &str = "2222222222222222222222222222222222222222222222222222222222222222";
// Failure-only watchdog. It never supplies cleanup, retirement, retry or auth success.
const WATCHDOG: Duration = Duration::from_secs(15);

#[derive(Default)]
pub(crate) struct Pause {
    state: Mutex<(bool, bool)>,
    changed: Condvar,
}
impl Pause {
    fn enter(&self) {
        let mut state = self.state.lock().expect("pause lock");
        state.0 = true;
        self.changed.notify_all();
        while !state.1 {
            state = self.changed.wait(state).expect("pause wait");
        }
    }
    fn release(&self) {
        self.state.lock().expect("pause lock").1 = true;
        self.changed.notify_all();
    }
    fn wait_entered(&self) {
        let state = self.state.lock().expect("pause lock");
        let (state, timed) = self
            .changed
            .wait_timeout_while(state, WATCHDOG, |s| !s.0)
            .expect("pause observation");
        assert!(
            state.0 && !timed.timed_out(),
            "actual operation did not reach pause"
        );
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum PipeFault {
    #[default]
    None,
    Error,
    Panic,
    Missing,
    Held,
}
#[derive(Default)]
pub(crate) struct Facts {
    pub material: Vec<(PathBuf, Value)>,
    pub argv: Vec<Vec<String>>,
    pub seeded_inputs: Vec<Option<String>>,
    pub requests: Vec<Value>,
    pub events: Vec<&'static str>,
    pub diagnostics: Vec<String>,
    pub outputs: Vec<OutputProbe>,
}

pub(crate) struct OutputProbe {
    tail: Arc<crate::engine::agent_runtime::OutputTail>,
    broadcast: Option<tokio::sync::broadcast::Receiver<Vec<u8>>>,
}

/// Test-only observer consumed by the planned AgentEngine test constructor.
/// Callbacks must be placed at their named actual operation, never on a scenario branch.
/// The same Arc is the existing external Apple test boundary below.
pub(crate) struct AuthObserver {
    pub facts: Mutex<Facts>,
    pub changed: Condvar,
    pub resource_drop: Pause,
    pub resource_live: AtomicBool,
    pub cleanup: Pause,
    pub hold_release: AtomicBool,
    pub hold_cleanup: AtomicBool,
    pub cleanup_unknown: AtomicBool,
    pub teardown_uncertain: AtomicBool,
    pub reject_after_plan: AtomicBool,
    pub seed_existing_config: AtomicBool,
    pub pipe_fault: Mutex<PipeFault>,
    pub pipe_pause: Arc<Pause>,
    dir: PathBuf,
    executable: &'static str,
    created: AtomicBool,
    cleaning: AtomicBool,
    removed: AtomicBool,
    created_at: Mutex<Option<chrono::DateTime<Utc>>>,
    name: Mutex<Option<String>>,
}
impl AuthObserver {
    pub(crate) fn event(&self, event: &'static str) {
        self.facts.lock().expect("facts").events.push(event);
        self.changed.notify_all();
    }
    pub(crate) fn wait_event(&self, event: &'static str) {
        let state = self.facts.lock().expect("facts");
        let (state, timed) = self
            .changed
            .wait_timeout_while(state, WATCHDOG, |facts| !facts.events.contains(&event))
            .expect("event observation");
        assert!(
            state.events.contains(&event) && !timed.timed_out(),
            "missing event: {event}"
        );
    }
    pub(crate) fn before_owner_material(&self, state: &Path) {
        let owner = state.parent().expect("owner directory");
        let active: Value = serde_json::from_slice(
            &fs::read(owner.join("active-run.json"))
                .expect("activation before actual material operation"),
        )
        .expect("active record");
        assert!(active.get("requested_session").is_some());
        assert!(active["requested_session"].is_null());
        self.facts
            .lock()
            .expect("facts")
            .material
            .push((state.to_path_buf(), active));
        if self.seed_existing_config.swap(false, Ordering::SeqCst) {
            fs::write(
                state.join("config.toml"),
                b"existing_config_must_survive = true\n",
            )
            .expect("fixture config exchange");
        }
    }
    // Test-only actual resource: insert this owned guard into the existing lifecycle
    // resource bundle before spawn; never drop it from an observer callback.
    pub(crate) fn take_launch_resource(self: &Arc<Self>) -> Option<Box<dyn std::any::Any + Send>> {
        if !self.hold_release.load(Ordering::SeqCst) {
            return None;
        }
        assert!(!self.resource_live.swap(true, Ordering::SeqCst));
        Some(Box::new(HeldLaunchResource {
            observer: Arc::clone(self),
        }))
    }
    pub(crate) fn observe_seeded_input(&self, prompt: Option<&str>) {
        self.facts
            .lock()
            .expect("facts")
            .seeded_inputs
            .push(prompt.map(str::to_owned));
    }
    pub(crate) fn observe_output_routes(
        &self,
        config: &crate::engine::container::io_bridge::BridgeConfig,
    ) {
        self.facts.lock().expect("facts").outputs.push(OutputProbe {
            tail: Arc::clone(&config.output_tail),
            broadcast: config
                .output_broadcast
                .as_ref()
                .map(|sender| sender.subscribe()),
        });
    }
    pub(crate) fn adapt_stderr(&self, pipe: Box<dyn Read + Send>) -> Option<Box<dyn Read + Send>> {
        match *self.pipe_fault.lock().expect("pipe fault") {
            PipeFault::None => Some(pipe),
            PipeFault::Missing => None,
            fault => Some(Box::new(FaultPipe {
                pipe,
                fault,
                pause: Arc::clone(&self.pipe_pause),
            })),
        }
    }
    pub(crate) fn state_path(&self) -> PathBuf {
        self.facts
            .lock()
            .expect("facts")
            .material
            .first()
            .expect("material operation")
            .0
            .clone()
    }
    pub(crate) fn activation(&self) -> Value {
        serde_json::from_slice(
            &fs::read(
                self.state_path()
                    .parent()
                    .expect("owner")
                    .join("active-run.json"),
            )
            .expect("activation"),
        )
        .expect("active JSON")
    }
    pub(crate) fn assert_locked(&self) {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(
                self.state_path()
                    .parent()
                    .expect("owner")
                    .join("writer.lock"),
            )
            .expect("lock file");
        assert!(
            matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock)),
            "actual writer authority was released while custody remained"
        );
    }
    pub(crate) fn assert_unlocked(&self) {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(
                self.state_path()
                    .parent()
                    .expect("owner")
                    .join("writer.lock"),
            )
            .expect("lock file");
        file.try_lock()
            .expect("positive control: exact writer is releasable");
        file.unlock().expect("unlock control descriptor");
    }
    pub(crate) fn assert_unpublished(&self) {
        let state = self.state_path();
        let owner = state.parent().expect("owner");
        assert!(!owner.join("completed-run.json").exists());
        let sessions = owner
            .parent()
            .expect("owners")
            .parent()
            .expect("namespace")
            .join("sessions");
        assert_eq!(fs::read_dir(sessions).expect("sessions").count(), 0);
    }
    fn release_all(&self) {
        self.resource_drop.release();
        self.cleanup.release();
        self.pipe_pause.release();
        self.cleanup_unknown.store(false, Ordering::SeqCst);
        let _ = fs::write(self.dir.join("child-release"), b"release");
        let _ = fs::write(self.dir.join("fixture-stop"), b"stop");
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
struct HeldLaunchResource {
    observer: Arc<AuthObserver>,
}
impl Drop for HeldLaunchResource {
    fn drop(&mut self) {
        self.observer.resource_drop.enter();
        self.observer.resource_live.store(false, Ordering::SeqCst);
    }
}
struct FaultPipe {
    pipe: Box<dyn Read + Send>,
    fault: PipeFault,
    pause: Arc<Pause>,
}
impl Read for FaultPipe {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        match self.fault {
            PipeFault::Error => Err(io::Error::other("synthetic actual pipe read error")),
            PipeFault::Panic => panic!("synthetic actual reader panic"),
            PipeFault::Held => {
                self.pause.enter();
                self.pipe.read(bytes)
            }
            PipeFault::None | PipeFault::Missing => self.pipe.read(bytes),
        }
    }
}

impl GatedProviderAdapter for AuthObserver {
    fn provider(&self) -> ProviderKind {
        ProviderKind::AppleContainers
    }
    fn resolve_image_identity(
        &self,
        _: &ImageRef,
        _: ProviderCallDeadline,
    ) -> Result<ImmutableImageId, LaunchIdentityError> {
        ImmutableImageId::new(format!("sha256:{}", "0".repeat(64)))
    }
    fn inspect_name_absence(
        &self,
        name: &ContainerName,
        _: ProviderCallDeadline,
    ) -> Result<AbsenceObservation, NamePresence> {
        if self.cleaning.load(Ordering::SeqCst) && self.cleanup_unknown.load(Ordering::SeqCst) {
            return Err(NamePresence::Unavailable);
        }
        if self.name.lock().expect("provider name").as_deref() == Some(name.as_str())
            && self.created.load(Ordering::SeqCst)
            && !self.removed.load(Ordering::SeqCst)
        {
            Err(NamePresence::Present)
        } else {
            Ok(self.absence(name))
        }
    }
    fn inspect_launch(&self, key: &ProviderLaunchKey, _: ProviderCallDeadline) -> ExactInspection {
        if self.cleaning.load(Ordering::SeqCst) && self.cleanup_unknown.load(Ordering::SeqCst) {
            return ExactInspection::Unavailable;
        }
        if self.removed.load(Ordering::SeqCst) {
            return ExactInspection::Absent(self.absence(&key.container_name));
        }
        let Some(created_at) = *self.created_at.lock().expect("created timestamp") else {
            return ExactInspection::Unavailable;
        };
        let state = ProviderState::Apple(if self.cleaning.load(Ordering::SeqCst) {
            AppleProviderState::Stopped
        } else {
            AppleProviderState::Running
        });
        let runtime_id = "a".repeat(64);
        let revision = canonical_inspection_revision(CanonicalInspectionRevisionInput {
            kind: InspectionObservationKind::Matching,
            provider: key.provider,
            exact_name: &key.container_name,
            runtime_id: Some(&runtime_id),
            token_digest: Some(key.token_digest),
            immutable_image_id: Some(&key.immutable_image_id),
            created_at: Some(created_at),
            state: SanitizedProviderStateObservation::Known(state.clone()),
        });
        ExactInspection::Matching(ProviderLaunchInspection {
            provider: key.provider,
            runtime_id,
            exact_name: key.container_name.clone(),
            token_digest: key.token_digest,
            immutable_image_id: key.immutable_image_id.clone(),
            created_at,
            state,
            revision,
        })
    }
    fn classify_create_exit(&self, _: &CreateCliExitEvidence) -> CreateCliExitClass {
        CreateCliExitClass::Other
    }
    fn stop_inspected(
        &self,
        _: &ProviderLaunchInspection,
        _: ProviderCallDeadline,
    ) -> Result<(), EngineError> {
        self.event("provider-stop");
        Ok(())
    }
    fn remove_inspected(
        &self,
        _: &ProviderLaunchInspection,
        _: ProviderCallDeadline,
    ) -> Result<(), EngineError> {
        if self.hold_cleanup.load(Ordering::SeqCst) {
            self.cleanup.enter();
        }
        self.removed.store(true, Ordering::SeqCst);
        self.event("exact-remove");
        Ok(())
    }
}

/// External provider view and simulated guest READY only. The local script is the actual child.
impl OrchestratedAppleTestBoundary for Arc<AuthObserver> {
    fn retention_available(&self) -> bool {
        true
    }
    fn image_inspection(&self) -> Result<Vec<u8>, EngineError> {
        serde_json::to_vec(&json!([{"variants":[{"config":{"config":{
            "User":"1000:1000", "Env":[], "Entrypoint":[],
            "Labels":{"dev.awman.startup-gate":"1"}
        }}}]}]))
        .map_err(|e| EngineError::Container(e.to_string()))
    }
    fn provider(&self) -> Arc<dyn GatedProviderAdapter> {
        Arc::clone(self) as Arc<dyn GatedProviderAdapter>
    }
    fn executable(&self) -> &'static str {
        self.executable
    }
    fn prepare_simulated_guest(
        &self,
        plan: &DurableLaunchPlan,
    ) -> Result<(), LaunchRetentionReason> {
        let request = serde_json::to_value(plan.controls.request.request.as_ref())
            .map_err(|_| LaunchRetentionReason::PostSpawnControlChanged)?;
        self.facts.lock().expect("facts").requests.push(request);
        let bindings = plan.controls.request.request.bindings.iter().map(|b| json!({
            "id":b.id,"manifest_id":b.manifest_id,"access":b.access,"workspace_path":b.workspace_path
        })).collect::<Vec<_>>();
        let bytes = serde_json::to_vec(&json!({"version":2,"nonce":NONCE,
            "container_name":plan.key.container_name.as_str(),"bindings":bindings,"guest_cwd":"/workspace"}))
            .map_err(|_| LaunchRetentionReason::PostSpawnControlChanged)?;
        plan.controls
            .publish_guest_record("ready.json", ".ready", &bytes)
            .map_err(|_| LaunchRetentionReason::PostSpawnControlChanged)?;
        Ok(())
    }
    fn pause_before_ready(&self) -> bool {
        false
    }
    fn pause_after_ready(&self) -> bool {
        false
    }
    fn observe_spawn(
        &self,
        argv: Vec<String>,
        name: String,
        _: Vec<String>,
        _: Vec<(String, bool)>,
        registry: bool,
        _: Vec<u8>,
        _: BTreeMap<String, Vec<u8>>,
    ) {
        assert!(
            registry,
            "existing retention registry must reach actual spawn"
        );
        *self.name.lock().expect("provider name") = Some(name);
        *self.created_at.lock().expect("created timestamp") = None;
        self.created.store(false, Ordering::SeqCst);
        self.cleaning.store(false, Ordering::SeqCst);
        self.removed.store(false, Ordering::SeqCst);
        self.facts.lock().expect("facts").argv.push(argv);
        self.assert_locked();
        self.event("spawn");
    }
    fn observe_child_start(&self) {
        *self.created_at.lock().expect("created timestamp") = Some(Utc::now());
        self.created.store(true, Ordering::SeqCst);
        self.event("child-start");
    }
    fn observe_protected_launch_identity(&self, _: &ProviderLaunchInspection) {
        self.event("exact-ready");
    }
    fn observe_ready_rejected(&self) {
        self.event("ready-rejected");
    }
    fn observe_protected_ready(&self) {
        self.event("protected-ready");
    }
    fn observe_accepted_release(&self) {
        self.event("protected-release");
    }
    fn observe_provider_entry(&self, _: bool) {
        self.event("provider-entry");
    }
    fn observe_cancel_point(&self, _: TestCancellationPoint) {}
    fn before_cli_start_after_plan(&self) -> bool {
        self.reject_after_plan.load(Ordering::SeqCst)
    }
    fn begin_cleanup(&self) {
        self.cleaning.store(true, Ordering::SeqCst);
    }
    fn observe_reap(&self) {
        self.event("reap-attempt");
    }
    fn observe_actual_child_reaped(&self) {
        self.event("actual-reap");
    }
    fn observe_never_created_proven(&self) {
        self.event("never-created");
    }
    fn observe_exact_absence_proven(&self) {
        self.event("exact-absence");
    }
    fn observe_cleanup_publication(&self, succeeded: bool) {
        if succeeded {
            self.event("cleanup-published");
        }
    }
    fn observe_resource_loan_released(&self) {
        self.event("resources-released");
    }
    fn cleanup_reap_proven(&self) -> bool {
        true
    }
    fn observe_custody(&self, facts: TestOwnershipObservation, _: bool) {
        if facts.registry_ticket {
            self.event("retained");
        }
    }
}

pub(crate) struct Fixture {
    pub root: Option<tempfile::TempDir>,
    registry: Arc<LaunchRetentionRegistry>,
    probe: retention_test_support::RetentionProbe,
    _supervisor_barrier: retention_test_support::SupervisorActiveClaimBarrier,
    pub observer: Arc<AuthObserver>,
    pub session: Session,
    pub runtime: Arc<ContainerRuntime>,
    pub overlay: Arc<OverlayEngine>,
    pub engine: AgentEngine,
}
impl Fixture {
    pub(crate) fn new(extra_env: &[(&str, &str)]) -> TestResult<Self> {
        let home_root =
            dirs::home_dir().ok_or_else(|| io::Error::other("test requires existing HOME"))?;
        let root = tempfile::Builder::new()
            .prefix("awman-auth-packet-")
            .tempdir_in(home_root)?;
        let project = root.path().join("project");
        let home = root.path().join("config");
        fs::create_dir(&project)?;
        fs::create_dir(&home)?;
        fs::create_dir(home.join("data"))?;
        fs::create_dir(project.join(".awman"))?;
        fs::write(project.join(".awman/config.json"), b"{\"auth\":\"none\"}")?;
        let mut env = vec![(
            "AWMAN_CONFIG_HOME".to_string(),
            home.to_string_lossy().into_owned(),
        )];
        env.extend(
            extra_env
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string())),
        );
        let session = Session::open_at_git_root(
            project.clone(),
            project,
            SessionOpenOptions {
                env: Some(EnvSnapshot::with_overrides(env)),
                available_agents: Some(vec![AgentName::new("codex")?]),
                ..Default::default()
            },
        )?;
        let executable_path = root.path().join("container-fixture");
        let quote = |path: &Path| format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"));
        let script = format!(
            r##"#!/bin/sh
control=''
prev=''
for arg in "$@"; do
  if [ "$prev" = '-v' ]; then
    case "$arg" in *:/.awman/startup-gate/control) control=${{arg%:/.awman/startup-gate/control}} ;; esac
  fi
  prev=$arg
done
[ -n "$control" ] || exit 91
while [ ! -f "$control/release.json" ]; do
  [ ! -f {stop} ] || exit 92
  sleep 0.01
done
released_tmp="$control/.released.tmp.$$"
printf '%s' '{{"nonce":"{NONCE}","version":1}}' > "$released_tmp"
mv "$released_tmp" "$control/.released"
printf '%s\n' '{URL}'
printf '%s\n' 'SYNTHETIC_STDERR_CHALLENGE' >&2
if [ -f {bulk} ]; then
  i=0
  while [ "$i" -lt 8192 ]; do
    printf '%s' 'synthetic-padding-to-force-real-reader-drain-after-user-sink-loss-'
    i=$((i+1))
  done
fi
while [ ! -f {release} ]; do
  [ ! -f {stop} ] || exit 93
  sleep 0.01
done
if [ -f {exit_code} ]; then code=$(cat {exit_code}); else code=0; fi
exit "$code"
"##,
            stop = quote(&root.path().join("fixture-stop")),
            release = quote(&root.path().join("child-release")),
            exit_code = quote(&root.path().join("exit-code")),
            bulk = quote(&root.path().join("bulk-output"))
        );
        fs::write(&executable_path, script)?;
        fs::set_permissions(&executable_path, fs::Permissions::from_mode(0o700))?;
        let executable: &'static str = Box::leak(
            executable_path
                .to_string_lossy()
                .into_owned()
                .into_boxed_str(),
        );
        let observer = Arc::new(AuthObserver {
            facts: Mutex::new(Facts::default()),
            changed: Condvar::new(),
            resource_drop: Pause::default(),
            resource_live: AtomicBool::new(false),
            cleanup: Pause::default(),
            hold_release: AtomicBool::new(false),
            hold_cleanup: AtomicBool::new(false),
            cleanup_unknown: AtomicBool::new(false),
            teardown_uncertain: AtomicBool::new(false),
            reject_after_plan: AtomicBool::new(false),
            seed_existing_config: AtomicBool::new(false),
            pipe_fault: Mutex::new(PipeFault::None),
            pipe_pause: Arc::new(Pause::default()),
            dir: root.path().to_path_buf(),
            executable,
            created: AtomicBool::new(false),
            cleaning: AtomicBool::new(false),
            removed: AtomicBool::new(false),
            created_at: Mutex::new(None),
            name: Mutex::new(None),
        });
        let runtime = Arc::new(ContainerRuntime::apple_with_test_boundary(Arc::new(
            Arc::clone(&observer),
        )));
        let overlay = Arc::new(
            OverlayEngine::with_auth_resolver(AuthPathResolver::at_home(&home))
                .with_secret_files_provider(Arc::new(|_| {
                    panic!("generic secret discovery reached by strict route")
                })),
        );
        // New test-only seam: fixed synthetic guest/profile inputs and observation/pipe pauses only.
        // It must not construct a lease, accept cleanup claims or replace production orchestration.
        let engine = AgentEngine::new(Arc::clone(&overlay), Arc::clone(&runtime))
            .with_native_codex_auth_test_observer(Arc::clone(&observer));
        let registry = runtime
            .launch_retention()
            .map_err(|_| io::Error::other("fixture registry unavailable"))?;
        let probe = retention_test_support::registry_probe(&registry);
        let supervisor_barrier = probe.hold_supervisor_active_claims();
        Ok(Self {
            root: Some(root),
            observer,
            session,
            runtime,
            overlay,
            engine,
            registry,
            probe,
            _supervisor_barrier: supervisor_barrier,
        })
    }
    pub(crate) fn path(&self) -> &Path {
        self.root.as_ref().expect("fixture root").path()
    }
    pub(crate) fn request(&self) -> TestResult<NativePrintRequest> {
        Ok(NativePrintRequest::new(
            "codex",
            Some("explicit-fixture-model"),
            None,
        )?)
    }
    pub(crate) fn release_child(&self) {
        fs::write(self.path().join("child-release"), b"release").expect("release fixture child");
    }
    pub(crate) fn frontend(&self) -> (Box<dyn AgentFrontend>, UserOutput) {
        let (out, out_rx) = tokio::sync::mpsc::unbounded_channel();
        let (err, err_rx) = tokio::sync::mpsc::unbounded_channel();
        (
            Box::new(UserFrontend {
                observer: Arc::clone(&self.observer),
                out,
                err,
            }),
            UserOutput { out_rx, err_rx },
        )
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.observer.release_all();
        let deadline = Instant::now() + WATCHDOG;
        let mut recovered_resources = 0;
        while self.probe.snapshot().retained != 0 && Instant::now() < deadline {
            if let Some(ticket) = self.probe.only_ticket() {
                if matches!(
                    self.registry.retry_active_gated(ticket, deadline),
                    Ok(ActiveGatedRetryOutcome::Resolved)
                ) {
                    recovered_resources += self.probe.resource_loan_releases(ticket);
                }
            }
            let _ = self.probe.advance_one_supervisor_active_attempt();
            std::thread::yield_now();
        }
        let state = self.probe.snapshot();
        let facts = self.observer.facts.lock().expect("facts");
        let starts = facts
            .events
            .iter()
            .filter(|event| **event == "child-start")
            .count();
        let released = facts
            .events
            .iter()
            .filter(|event| **event == "resources-released")
            .count()
            + recovered_resources;
        let cleaned = facts
            .events
            .iter()
            .filter(|event| **event == "cleanup-published")
            .count();
        let reaped = facts
            .events
            .iter()
            .filter(|event| **event == "actual-reap")
            .count();
        let unknown = self.observer.teardown_uncertain.load(Ordering::SeqCst)
            || self.observer.resource_live.load(Ordering::SeqCst)
            || starts > released
            || starts > cleaned
            || starts > reaped;
        drop(facts);
        if unknown || state.retained != 0 || state.unreaped != 0 {
            let kept = self.root.take().expect("fixture root").keep();
            if !std::thread::panicking() {
                panic!(
                    "fixture recovery did not retire custody; retained fixture at {}",
                    kept.display()
                );
            }
        }
    }
}

pub(crate) struct UserOutput {
    out_rx: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    err_rx: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
}
impl UserOutput {
    pub(crate) fn bytes(&mut self) -> Vec<u8> {
        let mut bytes = Vec::new();
        while let Ok(chunk) = self.out_rx.try_recv() {
            bytes.extend(chunk);
        }
        while let Ok(chunk) = self.err_rx.try_recv() {
            bytes.extend(chunk);
        }
        bytes
    }
}
struct UserFrontend {
    observer: Arc<AuthObserver>,
    out: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    err: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
}
impl UserMessageSink for UserFrontend {
    fn write_message(&mut self, msg: UserMessage) {
        self.observer
            .facts
            .lock()
            .expect("facts")
            .diagnostics
            .push(msg.text);
    }
    fn replay_queued(&mut self) {}
}
#[async_trait::async_trait]
impl AgentFrontend for UserFrontend {
    fn report_status(&mut self, status: AgentStatus) {
        self.observer
            .facts
            .lock()
            .expect("facts")
            .diagnostics
            .push(format!("{status:?}"));
    }
    fn report_progress(&mut self, progress: AgentProgress) {
        self.observer
            .facts
            .lock()
            .expect("facts")
            .diagnostics
            .push(progress.message);
    }
    fn take_io(&mut self) -> AgentIo {
        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
        AgentIo {
            stdout: self.out.clone(),
            stderr: self.err.clone(),
            stdin_tx,
            stdin_rx,
            resize: None,
            initial_size: None,
        }
    }
}

fn prepare(f: &Fixture) -> super::native_codex_preparation::NativeCodexPreparation {
    match f
        .engine
        .prepare_native_codex(&f.session, f.request().expect("request"))
    {
        Ok(owner) => owner,
        Err(_) => panic!("eligible fresh fixture must activate and prepare"),
    }
}
fn start(
    f: &Fixture,
    user: Box<dyn AgentFrontend>,
) -> super::native_codex_preparation::NativeCodexAuthExecution {
    match prepare(f).start_auth(user) {
        Ok(exec) => exec,
        Err(_) => panic!("actual fixture auth must start"),
    }
}
struct Waiter {
    task: Option<tokio::task::JoinHandle<AuthRetirement>>,
    cancel: crate::engine::agent_runtime::CancelHandle,
    observer: Arc<AuthObserver>,
}
impl Waiter {
    fn new(
        exec: super::native_codex_preparation::NativeCodexAuthExecution,
        observer: Arc<AuthObserver>,
    ) -> Self {
        let cancel = exec.cancel_handle();
        Self {
            task: Some(tokio::spawn(async move { exec.wait_auth_retired().await })),
            cancel,
            observer,
        }
    }
    fn is_finished(&self) -> bool {
        self.task.as_ref().expect("wait task").is_finished()
    }
    async fn finish(mut self) -> AuthRetirement {
        let result = tokio::time::timeout(WATCHDOG, self.task.as_mut().expect("wait task"))
            .await
            .expect("test watchdog: retirement did not finish")
            .expect("wait task panicked");
        self.task.take();
        result
    }
}
impl Drop for Waiter {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            // Abort is not proof the future transferred its lease to the registry yet.
            self.observer
                .teardown_uncertain
                .store(true, Ordering::SeqCst);
            self.observer.release_all();
            let _ = self.cancel.cancel();
            task.abort();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_retirement_requires_actual_child_readers_resources_and_exact_cleanup() -> TestResult {
    for barrier in ["child", "readers", "resources", "cleanup"] {
        let f = Fixture::new(&[])?;
        let (user, _output) = f.frontend();
        if barrier == "readers" {
            *f.observer.pipe_fault.lock().expect("pipe fault") = PipeFault::Held;
        }
        f.observer
            .hold_release
            .store(barrier == "resources", Ordering::SeqCst);
        f.observer
            .hold_cleanup
            .store(barrier == "cleanup", Ordering::SeqCst);
        let exec = start(&f, user);
        let activation = f.observer.activation();
        let waiter = Waiter::new(exec, Arc::clone(&f.observer));
        f.observer.wait_event("protected-release");
        if barrier != "child" {
            f.release_child();
        }
        match barrier {
            "readers" => f.observer.pipe_pause.wait_entered(),
            "resources" => {
                f.observer.resource_drop.wait_entered();
                assert!(f.observer.resource_live.load(Ordering::SeqCst));
            }
            "cleanup" => f.observer.cleanup.wait_entered(),
            _ => (),
        }
        assert!(!waiter.is_finished());
        f.observer.assert_locked();
        f.observer.assert_unpublished();
        f.release_child();
        f.observer.pipe_pause.release();
        f.observer.resource_drop.release();
        f.observer.cleanup.release();
        let AuthRetirement::Authenticated(retired) = waiter.finish().await else {
            panic!("all actual predicates must return original retired auth owner");
        };
        assert_eq!(f.observer.activation(), activation);
        f.observer.assert_locked();
        f.observer.assert_unpublished();
        let facts = f.observer.facts.lock().expect("facts");
        assert_eq!(facts.argv.len(), 1);
        for event in [
            "actual-reap",
            "exact-remove",
            "exact-absence",
            "cleanup-published",
            "resources-released",
        ] {
            assert!(facts.events.contains(&event), "retired without {event}");
        }
        drop(facts);
        assert!(!f.observer.resource_live.load(Ordering::SeqCst));
        drop(retired);
        f.observer.assert_unlocked();
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_cleanup_and_dropped_auth_keep_original_custody_unpublished() -> TestResult {
    for drop_without_wait in [false, true] {
        let f = Fixture::new(&[])?;
        let (user, _output) = f.frontend();
        f.observer.cleanup_unknown.store(true, Ordering::SeqCst);
        let exec = start(&f, user);
        f.observer.wait_event("protected-release");
        let activation = f.observer.activation();
        f.release_child();
        if drop_without_wait {
            drop(exec);
        } else {
            let AuthRetirement::Retained {
                ticket: _ticket,
                error: _error,
            } = Waiter::new(exec, Arc::clone(&f.observer)).finish().await
            else {
                panic!("unknown exact cleanup must retain, never authenticate/retry/stop");
            };
        }
        f.observer.wait_event("retained");
        let ticket = f
            .probe
            .only_ticket()
            .expect("exactly one actual registry ticket");
        assert!(f.probe.contains(ticket));
        f.observer.assert_locked();
        f.observer.assert_unpublished();
        assert_eq!(f.observer.activation(), activation);
        assert_eq!(f.observer.facts.lock().expect("facts").argv.len(), 1);
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_clean_cancellation_can_retry_same_activation_but_unqualified_exit_stops(
) -> TestResult {
    let f = Fixture::new(&[])?;
    let (user, _output) = f.frontend();
    let exec = start(&f, user);
    f.observer.wait_event("protected-release");
    let activation = f.observer.activation();
    // New narrow operation: delegates to the actual lifecycle cancel handle, not a status flag.
    exec.cancel_handle().cancel()?;
    let AuthRetirement::Retryable(retry) =
        Waiter::new(exec, Arc::clone(&f.observer)).finish().await
    else {
        panic!("actual explicit cancellation plus clean retirement permits initial retry");
    };
    f.observer.assert_locked();
    f.observer.assert_unpublished();
    assert!(f
        .observer
        .facts
        .lock()
        .expect("facts")
        .events
        .contains(&"actual-reap"));
    assert_eq!(f.observer.activation(), activation);
    fs::write(f.path().join("exit-code"), b"7")?;
    f.release_child();
    let (user, _output) = f.frontend();
    let exec = match retry.retry_initial_auth(user) {
        Ok(exec) => exec,
        Err(_) => panic!("clean initial retry must use original owner"),
    };
    let AuthRetirement::Stopped { owner, error: _ } =
        Waiter::new(exec, Arc::clone(&f.observer)).finish().await
    else {
        panic!("unqualified numeric exit is not provider expiration or retry authority");
    };
    assert_eq!(f.observer.activation(), activation);
    f.observer.assert_locked();
    f.observer.assert_unpublished();
    let facts = f.observer.facts.lock().expect("facts");
    assert_eq!(facts.argv.len(), 2);
    let original_state = facts
        .material
        .first()
        .expect("original material operation")
        .0
        .clone();
    assert!(
        facts
            .material
            .iter()
            .all(|(path, record)| path == &original_state && record == &activation),
        "retry must not acquire a different owner or activate a new run"
    );
    assert_eq!(
        facts.events.iter().filter(|e| **e == "actual-reap").count(),
        2
    );
    drop(facts);
    drop(owner);
    f.observer.assert_unlocked();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn private_auth_output_and_required_pipe_failures_never_grant_handoff() -> TestResult {
    for (fault, sink_lost) in [
        (PipeFault::None, false),
        (PipeFault::None, true),
        (PipeFault::Error, false),
        (PipeFault::Panic, false),
        (PipeFault::Missing, false),
        (PipeFault::Held, false),
    ] {
        let f = Fixture::new(&[])?;
        *f.observer.pipe_fault.lock().expect("fault") = fault;
        let (user, output) = f.frontend();
        let mut output = Some(output);
        if sink_lost {
            drop(output.take());
            fs::write(f.path().join("bulk-output"), b"fill pipe beyond capacity")?;
        }
        let exec = start(&f, user);
        let waiter = Waiter::new(exec, Arc::clone(&f.observer));
        f.observer.wait_event("protected-release");
        if fault == PipeFault::Held {
            f.observer.pipe_pause.wait_entered();
            f.release_child();
            assert!(!waiter.is_finished());
            f.observer.assert_locked();
            f.observer.pipe_pause.release();
        } else {
            f.release_child();
        }
        let outcome = waiter.finish().await;
        if matches!(fault, PipeFault::None | PipeFault::Held) {
            assert!(
                matches!(outcome, AuthRetirement::Authenticated(_)),
                "clean real readers permit handoff"
            );
            if let Some(output) = output.as_mut() {
                let bytes = output.bytes();
                for sentinel in [URL, "SYNTHETIC_STDERR_CHALLENGE"] {
                    assert!(
                        bytes
                            .windows(sentinel.len())
                            .any(|w| w == sentinel.as_bytes()),
                        "interactive sink lost real stream sentinel"
                    );
                }
            }
            assert!(f
                .observer
                .facts
                .lock()
                .expect("facts")
                .events
                .contains(&"actual-reap"));
        } else {
            assert!(
                matches!(outcome, AuthRetirement::Retained { .. }),
                "missing/error/panicked required reader must retain"
            );
            f.observer.assert_locked();
            let ticket = f
                .probe
                .only_ticket()
                .expect("actual reader-failure retention");
            assert!(f.probe.contains(ticket));
        }
        f.observer.assert_unpublished();
        let mut facts = f.observer.facts.lock().expect("facts");
        assert_eq!(
            facts.outputs.len(),
            1,
            "must observe actual bridge channels before readers start"
        );
        for probe in &mut facts.outputs {
            assert!(
                probe.tail.snapshot_text().is_empty(),
                "auth reached actual generic output tail"
            );
            if let Some(receiver) = probe.broadcast.as_mut() {
                assert!(
                    matches!(
                        receiver.try_recv(),
                        Err(tokio::sync::broadcast::error::TryRecvError::Empty
                            | tokio::sync::broadcast::error::TryRecvError::Closed)
                    ),
                    "auth reached actual broadcast"
                );
            }
        }
        assert!(!facts.diagnostics.join("\n").contains(URL));
        assert!(!facts
            .diagnostics
            .join("\n")
            .contains("SYNTHETIC_STDERR_CHALLENGE"));
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_plan_noncreation_cleanup_uncertainty_retains_real_registry_custody() -> TestResult {
    let f = Fixture::new(&[])?;
    f.observer.reject_after_plan.store(true, Ordering::SeqCst);
    f.observer.cleanup_unknown.store(true, Ordering::SeqCst);
    let (user, _output) = f.frontend();
    let Err(AuthStartError::Retained { ticket, error: _ }) = prepare(&f).start_auth(user) else {
        panic!("published-plan cleanup uncertainty must not return a reusable preparation");
    };
    assert!(
        f.probe.contains(ticket),
        "ticket must name existing actual retained registry custody"
    );
    assert!(!f
        .observer
        .facts
        .lock()
        .expect("facts")
        .events
        .contains(&"child-start"));
    f.observer.assert_locked();
    f.observer.assert_unpublished();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exclusive_fresh_config_never_overwrites_existing_material() -> TestResult {
    let f = Fixture::new(&[])?;
    f.observer
        .seed_existing_config
        .store(true, Ordering::SeqCst);
    let Err(PreparationError::Activated { owner, error: _ }) =
        f.engine.prepare_native_codex(&f.session, f.request()?)
    else {
        panic!("exclusive material creation must return original stopped owner on existing config");
    };
    assert_eq!(
        fs::read(f.observer.state_path().join("config.toml"))?,
        b"existing_config_must_survive = true\n"
    );
    assert!(f.observer.facts.lock().expect("facts").argv.is_empty());
    f.observer.assert_locked();
    f.observer.assert_unpublished();
    drop(owner);
    f.observer.assert_unlocked();
    Ok(())
}
