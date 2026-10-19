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
const WRONG_NONCE: &str = "3333333333333333333333333333333333333333333333333333333333333333";
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
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .1 = true;
        self.changed.notify_all();
    }
    fn wait_entered(&self) {
        let state = self.state.lock().expect("pause lock");
        let (state, timed) = self
            .changed
            .wait_timeout_while(state, WATCHDOG, |s| !s.0)
            .expect("pause observation");
        let observed = state.0 && !timed.timed_out();
        drop(state);
        assert!(observed, "actual operation did not reach pause");
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
    pub grace_cancel_handlers: Vec<bool>,
    pub seeded_plan_conflicts: Vec<PathBuf>,
    pub auth_release_plans: Vec<DurableLaunchPlan>,
    pub auth_release_takes: Vec<bool>,
    pub auth_release_attempt_errors: Vec<bool>,
    pub hostile_release_metadata: Option<fs::Metadata>,
    final_substitution: Option<FinalAuthSubstitution>,
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
    pub pre_permission: Pause,
    pub hold_release: AtomicBool,
    pub hold_cleanup: AtomicBool,
    pub hold_cancel_return: AtomicBool,
    pub cancel_return: Pause,
    pub cleanup_unknown: AtomicBool,
    pub cleanup_unknown_after_reap: AtomicBool,
    auth_sync_fault: Mutex<Option<crate::data::startup_gate::AuthReleaseSyncFaultHandle>>,
    final_mutation: Mutex<Option<FinalAuthMutation>>,
    pub teardown_uncertain: AtomicBool,
    pub reject_after_plan: AtomicBool,
    pub seed_existing_config: AtomicBool,
    pub seed_existing_plan: AtomicBool,
    pub capture_auth_release: AtomicBool,
    pub reattempt_auth_release: AtomicBool,
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
    pub(crate) fn after_actual_auth_cancel_return(&self) {
        if self.hold_cancel_return.swap(false, Ordering::SeqCst) {
            self.cancel_return.enter();
        }
    }
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
        let observed = state.events.contains(&event) && !timed.timed_out();
        drop(state);
        if !observed {
            use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
            let (events, control) = {
                let facts = self
                    .facts
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let mut controls = facts
                    .argv
                    .last()
                    .into_iter()
                    .flat_map(|args| args.windows(2))
                    .filter(|pair| pair[0] == "-v")
                    .filter_map(|pair| pair[1].strip_suffix(":/.awman/startup-gate/control"));
                let control = controls
                    .next()
                    .filter(|_| controls.next().is_none())
                    .map(PathBuf::from);
                (facts.events.clone(), control)
            };
            let metadata = control.as_ref().map(|path| {
                fs::symlink_metadata(path.join(".released"))
                    .map(|m| {
                        (
                            m.is_file(),
                            m.is_dir(),
                            m.file_type().is_symlink(),
                            m.mode(),
                            m.nlink(),
                            m.uid(),
                            m.len(),
                        )
                    })
                    .map_err(|error| error.kind())
            });
            let read_error = control.as_ref().and_then(|path| {
                OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
                    .open(path.join(".released"))
                    .err()
                    .map(|error| error.kind())
            });
            let _ = io::Write::write_fmt(&mut io::stderr(), format_args!("auth event diagnostic: events={events:?} ack_metadata(file,dir,symlink,full_mode,nlink,uid,len)={metadata:?} euid={} ack_read_error={read_error:?}\n", nix::unistd::geteuid().as_raw()));
        }
        assert!(observed, "missing event: {event}");
    }
    // Observation must sit immediately beside the sole production Option::take.
    // `present` is the actual taken Option's is_some(), never a scenario result.
    pub(crate) fn observe_auth_release_take(&self, present: bool) {
        self.facts
            .lock()
            .expect("facts")
            .auth_release_takes
            .push(present);
    }
    // Invoke once after the real first publication, outside the take helper.
    // The closure re-enters that SAME helper with original owner/plan/nonce/cancel.
    // This callback neither constructs authority nor publishes caller permission.
    pub(crate) fn after_auth_release_attempt(
        &self,
        first_result: &Result<(), LaunchRetentionReason>,
        reattempt: impl FnOnce() -> Result<(), LaunchRetentionReason>,
    ) {
        // Borrow of the producer's actual computed result, never a scenario flag.
        self.facts
            .lock()
            .expect("facts")
            .auth_release_attempt_errors
            .push(first_result.is_err());
        if !self.reattempt_auth_release.swap(false, Ordering::SeqCst) {
            return;
        }
        if self.dir.join("replay-failed-auth-release").is_file() {
            self.event("failed-release-before-reattempt");
            self.wait_fixture_file("allow-failed-release-reattempt");
        } else {
            self.wait_fixture_file("release-consumed");
        }
        assert!(
            reattempt().is_err(),
            "spent original launch accepted a second release attempt"
        );
        self.event("release-replay-checked");
    }
    fn wait_fixture_file(&self, name: &str) {
        let deadline = Instant::now() + WATCHDOG;
        while !self.dir.join(name).is_file() {
            assert!(
                Instant::now() < deadline,
                "test watchdog: missing fixture marker {name}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn auth_release_plan(&self) -> DurableLaunchPlan {
        let facts = self.facts.lock().expect("facts");
        assert_eq!(facts.auth_release_plans.len(), 1);
        facts.auth_release_plans[0].clone()
    }
    fn auth_control_path(&self) -> PathBuf {
        let facts = self.facts.lock().expect("facts");
        assert_eq!(facts.argv.len(), 1);
        let sources = facts.argv[0]
            .windows(2)
            .filter(|pair| pair[0] == "-v")
            .filter_map(|pair| pair[1].strip_suffix(":/.awman/startup-gate/control"))
            .collect::<Vec<_>>();
        assert_eq!(sources.len(), 1, "exact actual auth control mount");
        PathBuf::from(sources[0])
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
    // Called immediately after actual auth control files are created, before
    // prepare_orchestrated_launch reads that real parent's existing plan.
    pub(crate) fn before_protected_launch_plan(&self, controls_parent: &Path) {
        if !self.seed_existing_plan.swap(false, Ordering::SeqCst) {
            return;
        }
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let path = controls_parent.join("launch-plan.json");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .expect("exclusive actual conflicting plan fixture");
        file.write_all(b"unresolved-auth-plan-fixture\n")
            .expect("write real conflicting plan");
        file.sync_all().expect("sync real conflicting plan");
        self.facts
            .lock()
            .expect("facts")
            .seeded_plan_conflicts
            .push(path);
    }
    pub(crate) fn observe_output_routes(
        &self,
        config: &crate::engine::container::io_bridge::BridgeConfig,
    ) {
        self.facts
            .lock()
            .expect("facts")
            .grace_cancel_handlers
            .push(config.cancel_on_grace_expired.is_some());
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
    pub(crate) fn assert_auth_output_private(&self) {
        let mut facts = self.facts.lock().expect("facts");
        assert_eq!(facts.outputs.len(), 1, "expected actual auth bridge probe");
        for probe in &mut facts.outputs {
            assert!(
                probe.tail.snapshot_text().is_empty(),
                "auth reached generic output tail"
            );
            if let Some(receiver) = probe.broadcast.as_mut() {
                assert!(
                    matches!(
                        receiver.try_recv(),
                        Err(tokio::sync::broadcast::error::TryRecvError::Empty
                            | tokio::sync::broadcast::error::TryRecvError::Closed)
                    ),
                    "auth reached generic broadcast"
                );
            }
        }
        for marker in [URL, "SYNTHETIC_STDERR_CHALLENGE"] {
            assert!(
                !facts
                    .diagnostics
                    .iter()
                    .any(|message| message.contains(marker)),
                "auth reached diagnostic messages"
            );
        }
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
        self.pre_permission.release();
        self.resource_drop.release();
        self.cleanup.release();
        self.cancel_return.release();
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
        if self.capture_auth_release.load(Ordering::SeqCst) {
            self.facts
                .lock()
                .expect("facts")
                .auth_release_plans
                .push(plan.clone());
        }
        if self.dir.join("fail-auth-release-sync").is_file() {
            let fault = crate::data::startup_gate::arm_auth_release_post_rename_sync_failure(
                &plan.controls,
            );
            *self.auth_sync_fault.lock().expect("auth sync fault") = Some(fault);
        }
        if self.dir.join("consume-during-auth-publication").is_file() {
            let observer = Arc::clone(self);
            let controls = Arc::clone(&plan.controls);
            // Missing cfg(test) seam: run once after the ACTUAL successful auth
            // release no-replace rename, before sync/held metadata/path reopen.
            crate::data::startup_gate::after_auth_release_noreplace_rename(
                &plan.controls,
                Box::new(move || {
                    let permission: Value = serde_json::from_slice(
                        &controls
                            .read_guest_record("release.json", 4096)
                            .expect("read actual just-published caller permission")
                            .expect("actual rename must precede observation"),
                    )
                    .expect("actual release schema");
                    assert_eq!(permission, json!({"nonce": NONCE, "version": 1}));
                    // Scheduling only: the real guest, never this hook, unlinks permission.
                    fs::write(observer.dir.join("consume-release"), b"guest may consume")
                        .expect("release fixture consumption barrier");
                    observer.wait_fixture_file("release-consumed");
                    assert!(controls
                        .read_guest_record("release.json", 4096)
                        .expect("observe actual guest consumption")
                        .is_none());
                    observer.event("guest-consumed-before-publication-return");
                    observer.wait_fixture_file("resume-auth-publication");
                }),
            );
        }
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
        let final_mutation = *self.final_mutation.lock().expect("final fixture mutation");
        if let Some(kind) = final_mutation {
            self.substitute_final_auth_input(kind);
        }
        if self.dir.join("hold-before-auth-permission").is_file() {
            self.pre_permission.enter();
        }
        if self.dir.join("seed-hostile-auth-release").is_file() {
            let path = self.auth_control_path().join("release.json");
            std::os::unix::fs::symlink(self.dir.join("hostile-release-sentinel"), &path)
                .expect("fixture hostile entry after actual host receipt");
            let metadata = fs::symlink_metadata(&path).expect("fixture original hostile identity");
            self.facts.lock().expect("facts").hostile_release_metadata = Some(metadata);
            self.event("hostile-release-seeded");
        }
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
        // Fault only the later external inspection, after actual lifecycle reap.
        if self.cleanup_unknown_after_reap.load(Ordering::SeqCst) {
            self.cleanup_unknown.store(true, Ordering::SeqCst);
        }
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
    expected_unresolved: Option<LaunchRetentionTicket>,
    expected_publication_failure: Option<LaunchRetentionTicket>,
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
if [ -f {hold_ack} ]; then
  : > {release_visible}
  while [ ! -f {consume_release} ]; do
    [ ! -f {stop} ] || exit 94
    sleep 0.01
  done
  rm "$control/release.json" || exit 95
  : > {release_consumed}
  while [ ! -f {allow_ack} ]; do
    [ ! -f {stop} ] || exit 96
    sleep 0.01
  done
fi
released_tmp="$control/.released.tmp.$$"
printf '%s' '{{"nonce":"{NONCE}","version":1}}' > "$released_tmp"
if [ -f {wrong_ack} ]; then
  printf '%s' '{{"nonce":"{WRONG_NONCE}","version":1}}' > "$released_tmp"
fi
chmod 0600 "$released_tmp" || exit 97
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
            bulk = quote(&root.path().join("bulk-output")),
            wrong_ack = quote(&root.path().join("wrong-auth-ack")),
            hold_ack = quote(&root.path().join("hold-auth-ack")),
            release_visible = quote(&root.path().join("release-visible")),
            consume_release = quote(&root.path().join("consume-release")),
            release_consumed = quote(&root.path().join("release-consumed")),
            allow_ack = quote(&root.path().join("allow-auth-ack"))
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
            pre_permission: Pause::default(),
            hold_release: AtomicBool::new(false),
            hold_cleanup: AtomicBool::new(false),
            hold_cancel_return: AtomicBool::new(false),
            cancel_return: Pause::default(),
            cleanup_unknown: AtomicBool::new(false),
            cleanup_unknown_after_reap: AtomicBool::new(false),
            auth_sync_fault: Mutex::new(None),
            final_mutation: Mutex::new(None),
            teardown_uncertain: AtomicBool::new(false),
            reject_after_plan: AtomicBool::new(false),
            seed_existing_config: AtomicBool::new(false),
            seed_existing_plan: AtomicBool::new(false),
            capture_auth_release: AtomicBool::new(false),
            reattempt_auth_release: AtomicBool::new(false),
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
            expected_unresolved: None,
            expected_publication_failure: None,
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
        // Separate artifact keeper for the new actually-reaped publication case.
        // The existing no-child branch and its assertions below stay unchanged.
        if let Some(ticket) = self.expected_publication_failure {
            assert!(
                self.root.is_none(),
                "publication-failure root must already be kept"
            );
            self.assert_publication_failure_custody(ticket);
            return;
        }
        // Only the explicit expected-unresolved regression uses this branch.
        // The persistent root stays with real registry custody; this grants no retirement.
        if let Some(ticket) = self.expected_unresolved {
            assert!(
                self.root.is_none(),
                "expected unresolved artifact must already be kept"
            );
            assert!(
                self.probe.contains(ticket),
                "expected unresolved custody disappeared"
            );
            assert!(!self
                .observer
                .facts
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .events
                .contains(&"child-start"));
            return;
        }
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
        let facts = self
            .observer
            .facts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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

/// Artifact retention only. No custody extraction or process/cleanup authority.
pub(crate) struct ExpectedUnresolvedFixture {
    pub root: PathBuf,
    pub probe: retention_test_support::RetentionProbe,
    pub observer: Arc<AuthObserver>,
}
impl Fixture {
    pub(crate) fn assert_and_poll_unresolved(&self) -> LaunchRetentionTicket {
        let ticket = self
            .probe
            .only_ticket()
            .expect("actual unresolved preparation registry ticket");
        assert!(self.probe.contains(ticket));
        self.observer.assert_locked();
        assert!(
            matches!(
                self.registry
                    .retry_active_gated(ticket, Instant::now() + WATCHDOG),
                Ok(ActiveGatedRetryOutcome::Retained)
            ),
            "existing-plan uncertainty has no retry discharge authority"
        );
        self.probe
            .advance_one_supervisor_active_attempt()
            .expect("actual existing registry poll");
        assert!(self.probe.contains(ticket));
        self.observer.assert_locked();
        ticket
    }
    pub(crate) fn keep_expected_unresolved(
        mut self,
        ticket: LaunchRetentionTicket,
    ) -> ExpectedUnresolvedFixture {
        assert!(self.probe.contains(ticket));
        self.observer.assert_locked();
        assert!(!self
            .observer
            .facts
            .lock()
            .expect("facts")
            .events
            .contains(&"child-start"));
        let root = self.root.take().expect("fixture root").keep();
        let preserved = ExpectedUnresolvedFixture {
            root,
            probe: self.probe.clone(),
            observer: Arc::clone(&self.observer),
        };
        self.expected_unresolved = Some(ticket);
        // Drop real engine/runtime/registry handles. Shared must preserve unresolved custody.
        drop(self);
        preserved
    }
}
impl ExpectedUnresolvedFixture {
    pub(crate) fn assert_shutdown_retains(&self, ticket: LaunchRetentionTicket) {
        let deadline = Instant::now() + WATCHDOG;
        while !self.probe.snapshot().shutdown_requested && Instant::now() < deadline {
            self.probe.wake_supervisor();
            std::thread::yield_now();
        }
        assert!(
            self.probe.snapshot().shutdown_requested,
            "actual registry shutdown was never requested"
        );
        assert!(self.probe.contains(ticket));
        self.observer.assert_locked();
        assert!(
            self.root.exists(),
            "unresolved custody must retain its persistent filesystem inputs"
        );
        self.observer.assert_unpublished();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_bridge_has_no_installed_grace_cancel_but_real_user_cancel_still_works() -> TestResult
{
    let f = Fixture::new(&[])?;
    let (user, _output) = f.frontend();
    let exec = start(&f, user);
    let user_cancel = exec.cancel_handle();
    let waiter = Waiter::new(exec, Arc::clone(&f.observer));
    f.observer.wait_event("protected-release");
    assert_eq!(
        f.observer
            .facts
            .lock()
            .expect("facts")
            .grace_cancel_handlers,
        vec![false],
        "observe actual bridge config after install_lifecycle_cancel, not the earlier optout"
    );
    f.observer.assert_locked();
    user_cancel.cancel()?;
    let AuthRetirement::Retryable(retired) = waiter.finish().await else {
        panic!("explicit user cancellation still requires actual clean auth retirement");
    };
    for event in [
        "actual-reap",
        "exact-absence",
        "cleanup-published",
        "resources-released",
    ] {
        assert!(f
            .observer
            .facts
            .lock()
            .expect("facts")
            .events
            .contains(&event));
    }
    f.observer.assert_locked();
    f.observer.assert_unpublished();
    drop(retired);
    f.observer.assert_unlocked();
    Ok(())
}

// Caller-release packet: normal fixture behavior above is unchanged unless this
// explicit marker asks the real guest child to consume permission and hold ACK.
fn hold_real_auth_ack(f: &Fixture) -> TestResult {
    f.observer
        .capture_auth_release
        .store(true, Ordering::SeqCst);
    fs::write(
        f.path().join("hold-auth-ack"),
        b"hold only guest acknowledgment",
    )?;
    Ok(())
}
fn assert_real_auth_permission(f: &Fixture, waiter: &Waiter) -> TestResult {
    f.observer.wait_fixture_file("release-visible");
    let plan = f.observer.auth_release_plan();
    let control = f.observer.auth_control_path();
    let metadata = fs::symlink_metadata(control.join("release.json"))?;
    assert!(metadata.file_type().is_file());
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    let permission: Value = serde_json::from_slice(
        &plan
            .controls
            .read_guest_record("release.json", 4096)?
            .expect("real caller permission"),
    )?;
    assert_eq!(permission, json!({"nonce": NONCE, "version": 1}));
    let receipt: Value = serde_json::from_slice(&fs::read(
        control
            .parent()
            .expect("control parent")
            .join("launch.json"),
    )?)?;
    assert_eq!(receipt["nonce"], NONCE);
    assert_eq!(receipt["container_name"], plan.key.container_name.as_str());
    assert!(plan
        .controls
        .read_guest_record(".released", 4096)?
        .is_none());
    assert!(
        !waiter.is_finished(),
        "caller permission alone retired authentication"
    );
    assert!(!f
        .observer
        .facts
        .lock()
        .expect("facts")
        .events
        .contains(&"protected-release"));
    f.observer.assert_locked();
    f.observer.assert_unpublished();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_caller_permission_requires_actual_guest_ack_before_retirement() -> TestResult {
    let f = Fixture::new(&[])?;
    hold_real_auth_ack(&f)?;
    let (user, _output) = f.frontend();
    let waiter = Waiter::new(start(&f, user), Arc::clone(&f.observer));
    assert_real_auth_permission(&f, &waiter)?;
    assert_eq!(
        f.observer.facts.lock().expect("facts").auth_release_takes,
        vec![true]
    );
    fs::write(
        f.path().join("consume-release"),
        b"guest may consume actual permission",
    )?;
    f.observer.wait_fixture_file("release-consumed");
    assert!(f
        .observer
        .auth_release_plan()
        .controls
        .read_guest_record(".released", 4096)?
        .is_none());
    assert!(!waiter.is_finished());
    fs::write(f.path().join("allow-auth-ack"), b"guest may acknowledge")?;
    f.observer.wait_event("protected-release");
    f.release_child();
    let AuthRetirement::Authenticated(retired) = waiter.finish().await else {
        panic!("real permission, actual guest ACK and all retirement predicates must authenticate");
    };
    f.observer.assert_locked();
    f.observer.assert_unpublished();
    drop(retired);
    f.observer.assert_unlocked();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn consumed_auth_permission_cannot_rearm_same_launch_before_guest_ack() -> TestResult {
    let f = Fixture::new(&[])?;
    hold_real_auth_ack(&f)?;
    f.observer
        .reattempt_auth_release
        .store(true, Ordering::SeqCst);
    let (user, _output) = f.frontend();
    let waiter = Waiter::new(start(&f, user), Arc::clone(&f.observer));
    assert_real_auth_permission(&f, &waiter)?;
    fs::write(
        f.path().join("consume-release"),
        b"guest consumes real permission",
    )?;
    f.observer.wait_event("release-replay-checked");
    assert_eq!(
        f.observer.facts.lock().expect("facts").auth_release_takes,
        vec![true, false]
    );
    let plan = f.observer.auth_release_plan();
    assert!(
        plan.controls
            .read_guest_record("release.json", 4096)?
            .is_none(),
        "spent capability republished after guest consumption"
    );
    assert!(plan
        .controls
        .read_guest_record(".released", 4096)?
        .is_none());
    assert!(!waiter.is_finished());
    f.observer.assert_locked();
    f.observer.assert_unpublished();
    assert_eq!(f.observer.facts.lock().expect("facts").argv.len(), 1);
    fs::write(
        f.path().join("allow-auth-ack"),
        b"guest acknowledges first permission",
    )?;
    f.observer.wait_event("protected-release");
    f.release_child();
    let AuthRetirement::Authenticated(retired) = waiter.finish().await else {
        panic!("rejected replay must not fabricate a second launch or lose original auth custody");
    };
    f.observer.assert_locked();
    f.observer.assert_unpublished();
    drop(retired);
    f.observer.assert_unlocked();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_guest_consumption_before_host_publication_return_allows_actual_ack_retirement(
) -> TestResult {
    let f = Fixture::new(&[])?;
    hold_real_auth_ack(&f)?;
    fs::write(
        f.path().join("consume-during-auth-publication"),
        b"force real ordering",
    )?;
    let (user, _output) = f.frontend();
    let waiter = Waiter::new(start(&f, user), Arc::clone(&f.observer));
    f.observer
        .wait_event("guest-consumed-before-publication-return");
    let plan = f.observer.auth_release_plan();
    assert!(plan
        .controls
        .read_guest_record("release.json", 4096)?
        .is_none());
    assert!(plan
        .controls
        .read_guest_record(".released", 4096)?
        .is_none());
    assert_eq!(
        f.observer.facts.lock().expect("facts").auth_release_takes,
        vec![true]
    );
    assert!(!waiter.is_finished());
    f.observer.assert_locked();
    f.observer.assert_unpublished();
    fs::write(
        f.path().join("allow-auth-ack"),
        b"real guest may acknowledge consumed permission",
    )?;
    let deadline = Instant::now() + WATCHDOG;
    let acknowledgment = loop {
        if let Some(bytes) = plan.controls.read_guest_record(".released", 4096)? {
            break serde_json::from_slice::<Value>(&bytes)?;
        }
        assert!(
            Instant::now() < deadline,
            "test watchdog: actual guest ACK missing"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(acknowledgment, json!({"nonce": NONCE, "version": 1}));
    f.release_child();
    fs::write(
        f.path().join("resume-auth-publication"),
        b"host may finish actual publication",
    )?;
    // Direct terminal assertion: the known broken publisher returns retained
    // custody here. Do not disguise that target RED as a protected-release timeout.
    let retired = match waiter.finish().await {
        AuthRetirement::Authenticated(retired) => retired,
        AuthRetirement::Retained { ticket, error } => {
            assert!(f.probe.contains(ticket));
            panic!("legitimate guest consumption was treated as publication failure: {error:?}");
        }
        AuthRetirement::CustodyHeld { error, .. } => {
            panic!("legitimate guest consumption left unknown auth custody: {error:?}");
        }
        _ => panic!("actual valid ACK and successful child must authenticate after all retirement predicates"),
    };
    let facts = f.observer.facts.lock().expect("facts");
    assert_eq!(facts.argv.len(), 1);
    for event in [
        "protected-release",
        "actual-reap",
        "exact-absence",
        "cleanup-published",
        "resources-released",
    ] {
        assert!(
            facts.events.contains(&event),
            "race success without actual {event}"
        );
    }
    drop(facts);
    f.observer.assert_locked();
    f.observer.assert_unpublished();
    drop(retired);
    f.observer.assert_unlocked();
    Ok(())
}

// Artifact retention only; do not reuse or weaken the old no-child keeper.
impl Fixture {
    fn assert_publication_failure_custody(&self, ticket: LaunchRetentionTicket) {
        assert!(self.probe.contains(ticket));
        let (starts, reaps, cleaned, released, accepted) = {
            let facts = self.observer.facts.lock().expect("facts");
            let count = |event| facts.events.iter().filter(|item| **item == event).count();
            (
                count("child-start"),
                count("actual-reap"),
                count("cleanup-published"),
                count("resources-released"),
                count("protected-release"),
            )
        };
        assert_eq!(
            (starts, reaps),
            (1, 1),
            "keeper requires actual retirement of one real child"
        );
        assert_eq!((cleaned, released, accepted), (0, 0, 0));
        assert_eq!(self.probe.snapshot().unreaped, 0);
        assert_eq!(self.probe.active_gated_resource_loans(ticket), 1);
        assert_eq!(self.probe.resource_loan_releases(ticket), 0);
        self.observer.assert_locked();
        self.observer.assert_unpublished();
    }
    fn reap_publication_failure(&self, ticket: LaunchRetentionTicket) -> TestResult {
        assert!(matches!(
            self.registry
                .retry_active_gated(ticket, Instant::now() + WATCHDOG)?,
            ActiveGatedRetryOutcome::Retained
        ));
        self.observer.wait_event("actual-reap");
        self.assert_publication_failure_custody(ticket);
        Ok(())
    }
    fn keep_expected_publication_failure(
        mut self,
        ticket: LaunchRetentionTicket,
    ) -> ExpectedUnresolvedFixture {
        self.assert_publication_failure_custody(ticket);
        let root = self.root.take().expect("fixture root").keep();
        let preserved = ExpectedUnresolvedFixture {
            root,
            probe: self.probe.clone(),
            observer: Arc::clone(&self.observer),
        };
        self.expected_publication_failure = Some(ticket);
        drop(self); // actual last external engine/runtime/registry owners
        preserved
    }
}
fn prepare_failed_release_replay(f: &Fixture) -> TestResult {
    hold_real_auth_ack(f)?;
    fs::write(
        f.path().join("replay-failed-auth-release"),
        b"observe actual failed dispatch",
    )?;
    f.observer
        .reattempt_auth_release
        .store(true, Ordering::SeqCst);
    f.observer
        .cleanup_unknown_after_reap
        .store(true, Ordering::SeqCst);
    Ok(())
}
fn permit_failed_release_replay(f: &Fixture) -> TestResult {
    assert_eq!(
        f.observer.facts.lock().expect("facts").auth_release_takes,
        vec![true]
    );
    fs::write(
        f.path().join("allow-failed-release-reattempt"),
        b"repeat original consuming dispatch",
    )?;
    f.observer.wait_event("release-replay-checked");
    assert_eq!(
        f.observer.facts.lock().expect("facts").auth_release_takes,
        vec![true, false]
    );
    assert!(f
        .observer
        .auth_release_plan()
        .controls
        .read_guest_record("release.json", 4096)?
        .is_none());
    assert!(f
        .observer
        .auth_release_plan()
        .controls
        .read_guest_record(".released", 4096)?
        .is_none());
    assert_eq!(f.observer.facts.lock().expect("facts").argv.len(), 1);
    f.observer.assert_locked();
    f.observer.assert_unpublished();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_release_sync_failure_keeps_spent_custody_through_registry_shutdown() -> TestResult {
    let f = Fixture::new(&[])?;
    prepare_failed_release_replay(&f)?;
    fs::write(
        f.path().join("fail-auth-release-sync"),
        b"inject reported sync failure",
    )?;
    fs::write(
        f.path().join("consume-during-auth-publication"),
        b"prove actual rename and guest unlink",
    )?;
    let (user, _output) = f.frontend();
    let exec = start(&f, user);
    let cancel = exec.cancel_handle();
    let waiter = Waiter::new(exec, Arc::clone(&f.observer));
    f.observer
        .wait_event("guest-consumed-before-publication-return");
    let activation = f.observer.activation();
    assert!(f
        .observer
        .auth_release_plan()
        .controls
        .read_guest_record("release.json", 4096)?
        .is_none());
    fs::write(
        f.path().join("resume-auth-publication"),
        b"reach actual post-rename sync fault",
    )?;
    f.observer.wait_event("failed-release-before-reattempt");
    let fired = f
        .observer
        .auth_sync_fault
        .lock()
        .expect("auth sync fault")
        .as_ref()
        .is_some_and(|fault| fault.did_fire());
    assert!(
        fired,
        "armed but untriggered fault is not publication uncertainty evidence"
    );
    permit_failed_release_replay(&f)?;
    let AuthRetirement::Retained { ticket, error: _ } = waiter.finish().await else {
        panic!("actual reported sync failure must retain original spent auth custody");
    };
    assert!(f.probe.contains(ticket));
    assert_eq!(f.observer.activation(), activation);
    cancel.cancel()?;
    drop(cancel);
    f.reap_publication_failure(ticket)?;
    let actual_controls = Arc::clone(&f.observer.auth_release_plan().controls);
    let preserved = f.keep_expected_publication_failure(ticket);
    preserved.assert_shutdown_retains(ticket);
    assert_eq!(preserved.observer.activation(), activation);
    let actual_mount = crate::data::startup_gate::revalidate_mount_source(&actual_controls)?;
    let actual_parent = actual_mount
        .parent()
        .ok_or_else(|| io::Error::other("actual control parent missing"))?;
    assert_eq!(
        fs::read(actual_parent.join("request.json"))?,
        actual_controls.request.request_bytes()
    );
    let receipt_metadata = fs::symlink_metadata(actual_parent.join("launch.json"))?;
    assert!(receipt_metadata.file_type().is_file());
    assert_eq!(receipt_metadata.permissions().mode() & 0o777, 0o600);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_existing_release_symlink_survives_and_failed_attempt_stays_spent() -> TestResult {
    use std::os::unix::fs::MetadataExt;
    let f = Fixture::new(&[])?;
    prepare_failed_release_replay(&f)?;
    let sentinel = f.path().join("hostile-release-sentinel");
    fs::write(&sentinel, b"HOSTILE_FIXTURE_SENTINEL_MUST_SURVIVE")?;
    fs::set_permissions(&sentinel, fs::Permissions::from_mode(0o600))?;
    fs::write(
        f.path().join("seed-hostile-auth-release"),
        b"actual no-replace collision",
    )?;
    let (user, _output) = f.frontend();
    let exec = start(&f, user);
    let cancel = exec.cancel_handle();
    let waiter = Waiter::new(exec, Arc::clone(&f.observer));
    f.observer.wait_event("failed-release-before-reattempt");
    let activation = f.observer.activation();
    let path = f.observer.auth_control_path().join("release.json");
    let original = f
        .observer
        .facts
        .lock()
        .expect("facts")
        .hostile_release_metadata
        .clone()
        .expect("real hostile entry before actual publication attempt");
    let current = fs::symlink_metadata(&path)?;
    assert!(current.file_type().is_symlink());
    assert_eq!(
        (
            current.dev(),
            current.ino(),
            current.mode(),
            current.nlink(),
            current.uid()
        ),
        (
            original.dev(),
            original.ino(),
            original.mode(),
            original.nlink(),
            original.uid()
        )
    );
    assert_eq!(fs::read_link(&path)?, sentinel);
    assert_eq!(
        fs::read(&sentinel)?,
        b"HOSTILE_FIXTURE_SENTINEL_MUST_SURVIVE"
    );
    assert_eq!(fs::metadata(&sentinel)?.permissions().mode() & 0o777, 0o600);
    f.observer.assert_locked();
    f.observer.assert_unpublished();
    // Only the fixture removes its hostile entry, after proving preservation.
    // Empty destination cannot remint permission for the original launch.
    fs::remove_file(&path)?;
    permit_failed_release_replay(&f)?;
    let AuthRetirement::Retained { ticket, error: _ } = waiter.finish().await else {
        panic!("hostile existing release must retain original spent auth custody");
    };
    assert!(f.probe.contains(ticket));
    assert_eq!(f.observer.activation(), activation);
    assert_eq!(
        fs::read(&sentinel)?,
        b"HOSTILE_FIXTURE_SENTINEL_MUST_SURVIVE"
    );
    cancel.cancel()?;
    drop(cancel);
    f.reap_publication_failure(ticket)?;
    let actual_controls = Arc::clone(&f.observer.auth_release_plan().controls);
    let preserved = f.keep_expected_publication_failure(ticket);
    preserved.assert_shutdown_retains(ticket);
    assert_eq!(preserved.observer.activation(), activation);
    let actual_mount = crate::data::startup_gate::revalidate_mount_source(&actual_controls)?;
    let actual_parent = actual_mount
        .parent()
        .ok_or_else(|| io::Error::other("actual control parent missing"))?;
    assert_eq!(
        fs::read(actual_parent.join("request.json"))?,
        actual_controls.request.request_bytes()
    );
    let receipt_metadata = fs::symlink_metadata(actual_parent.join("launch.json"))?;
    assert!(receipt_metadata.file_type().is_file());
    assert_eq!(receipt_metadata.permissions().mode() & 0o777, 0o600);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_auth_ack_nonce_never_authenticates_even_when_child_may_exit_zero() -> TestResult {
    let f = Fixture::new(&[])?;
    hold_real_auth_ack(&f)?;
    fs::write(
        f.path().join("wrong-auth-ack"),
        b"real guest emits wrong nonce",
    )?;
    fs::write(f.path().join("exit-code"), b"0")?;
    let (user, _output) = f.frontend();
    let waiter = Waiter::new(start(&f, user), Arc::clone(&f.observer));
    assert_real_auth_permission(&f, &waiter)?;
    fs::write(
        f.path().join("consume-release"),
        b"guest consumes actual permission",
    )?;
    f.observer.wait_fixture_file("release-consumed");
    // No child-exit barrier remains: accepting the bad ACK could authenticate.
    f.release_child();
    fs::write(
        f.path().join("allow-auth-ack"),
        b"guest publishes its actual bad ACK",
    )?;
    let rejected = waiter.finish().await;
    let plan = f.observer.auth_release_plan();
    let acknowledgment: Value = serde_json::from_slice(
        &plan
            .controls
            .read_guest_record(".released", 4096)?
            .expect("actual guest ACK"),
    )?;
    assert_eq!(acknowledgment, json!({"version": 1, "nonce": WRONG_NONCE}));
    let metadata = fs::symlink_metadata(f.observer.auth_control_path().join(".released"))?;
    assert!(metadata.file_type().is_file());
    assert_eq!(metadata.permissions().mode() & 0o7777, 0o600);
    let events = f.observer.facts.lock().expect("facts").events.clone();
    assert!(!events.contains(&"protected-release"));
    f.observer.assert_locked();
    f.observer.assert_unpublished();
    match rejected {
        AuthRetirement::Stopped { owner, error: _ } => {
            // Actual clean retirement keeps the rejected preparation until drop.
            drop(owner);
        }
        AuthRetirement::Retained { ticket, error: _ } => {
            // Gate rejection may beat the immediate reap observation. Preserve
            // actual custody until normal exact cleanup qualifies resolution.
            assert!(f.probe.contains(ticket));
            f.observer.assert_locked();
            assert!(matches!(
                f.registry
                    .retry_active_gated(ticket, Instant::now() + WATCHDOG)?,
                ActiveGatedRetryOutcome::Resolved
            ));
            assert!(!f.probe.contains(ticket));
        }
        AuthRetirement::Authenticated(_) => panic!("wrong actual nonce authenticated"),
        AuthRetirement::Retryable(_) => panic!("wrong ACK is not user cancellation or expiration"),
        AuthRetirement::CustodyHeld { error, .. } => {
            panic!("wrong ACK produced untracked custody instead of owner/ticket: {error:?}");
        }
    }
    let events = f.observer.facts.lock().expect("facts").events.clone();
    for event in [
        "actual-reap",
        "exact-absence",
        "cleanup-published",
        "resources-released",
    ] {
        assert!(
            events.contains(&event),
            "resolved rejection lacks actual {event}"
        );
    }
    assert!(!events.contains(&"protected-release"));
    f.observer.assert_unpublished();
    f.observer.assert_unlocked();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_user_cancel_before_auth_permission_spends_attempt_and_retains_custody() -> TestResult
{
    let f = Fixture::new(&[])?;
    prepare_failed_release_replay(&f)?;
    fs::write(
        f.path().join("hold-before-auth-permission"),
        b"pause after real protected receipt",
    )?;
    let (user, _output) = f.frontend();
    let exec = start(&f, user);
    let cancel = exec.cancel_handle();
    let waiter = Waiter::new(exec, Arc::clone(&f.observer));
    f.observer.pre_permission.wait_entered();
    let activation = f.observer.activation();
    let actual_controls = Arc::clone(&f.observer.auth_release_plan().controls);
    let actual_mount = crate::data::startup_gate::revalidate_mount_source(&actual_controls)?;
    let actual_parent = actual_mount
        .parent()
        .ok_or_else(|| io::Error::other("actual control parent missing"))?;
    let receipt: Value = serde_json::from_slice(&fs::read(actual_parent.join("launch.json"))?)?;
    assert_eq!(receipt["nonce"], NONCE);
    assert!(actual_controls
        .read_guest_record("release.json", 4096)?
        .is_none());
    assert!(actual_controls
        .read_guest_record(".released", 4096)?
        .is_none());
    assert!(f
        .observer
        .facts
        .lock()
        .expect("facts")
        .auth_release_takes
        .is_empty());
    f.observer.assert_locked();
    f.observer.assert_unpublished();
    cancel.cancel()?; // actual production handle, not a scenario cancellation flag
    f.observer.pre_permission.release();
    // Current production ordering verifies the still-valid receipt, then takes
    // once before the capability checks actual cancellation; no earlier cancel
    // branch exists between the paused callback and that dispatch.
    f.observer.wait_event("failed-release-before-reattempt");
    permit_failed_release_replay(&f)?;
    let AuthRetirement::Retained { ticket, error: _ } = waiter.finish().await else {
        panic!("cancelled pre-permission attempt must retain its spent original custody");
    };
    assert!(f.probe.contains(ticket));
    assert_eq!(f.observer.activation(), activation);
    drop(cancel);
    f.reap_publication_failure(ticket)?;
    let preserved = f.keep_expected_publication_failure(ticket);
    preserved.assert_shutdown_retains(ticket);
    assert_eq!(preserved.observer.activation(), activation);
    let actual_mount = crate::data::startup_gate::revalidate_mount_source(&actual_controls)?;
    let actual_parent = actual_mount
        .parent()
        .ok_or_else(|| io::Error::other("actual control parent missing"))?;
    assert_eq!(
        fs::read(actual_parent.join("request.json"))?,
        actual_controls.request.request_bytes()
    );
    assert!(fs::symlink_metadata(actual_parent.join("launch.json"))?
        .file_type()
        .is_file());
    assert!(actual_controls
        .read_guest_record("release.json", 4096)?
        .is_none());
    assert!(actual_controls
        .read_guest_record(".released", 4096)?
        .is_none());
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FinalAuthMutation {
    SdkState,
    ControlChild,
    Receipt,
}
#[derive(Clone)]
struct FinalAuthSubstitution {
    kind: FinalAuthMutation,
    target: PathBuf,
    saved: PathBuf,
    target_metadata: fs::Metadata,
    sdk_metadata: fs::Metadata,
    receipt_metadata: fs::Metadata,
    receipt_bytes: Vec<u8>,
    intent: Value,
}
impl AuthObserver {
    // Only fixture-owned paths derived from actual preparation/child argv.
    fn substitute_final_auth_input(&self, kind: FinalAuthMutation) {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let sdk = self.state_path();
        let control = self.auth_control_path();
        let receipt = control
            .parent()
            .expect("actual control parent")
            .join("launch.json");
        let target = match kind {
            FinalAuthMutation::SdkState => sdk.clone(),
            FinalAuthMutation::ControlChild => control,
            FinalAuthMutation::Receipt => receipt.clone(),
        };
        let saved = target.with_file_name(".fixture-original-final-input");
        let evidence = FinalAuthSubstitution {
            kind,
            target_metadata: fs::symlink_metadata(&target)
                .expect("actual original target identity"),
            sdk_metadata: fs::symlink_metadata(&sdk).expect("actual original SDK identity"),
            receipt_metadata: fs::symlink_metadata(&receipt)
                .expect("actual original receipt identity"),
            receipt_bytes: fs::read(&receipt).expect("actual original receipt bytes"),
            intent: self.activation(),
            target: target.clone(),
            saved: saved.clone(),
        };
        fs::rename(&target, &saved).expect("preserve fixture original namespace");
        if kind == FinalAuthMutation::Receipt {
            let mut replacement = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&target)
                .expect("exclusive same-byte receipt replacement");
            replacement
                .write_all(&evidence.receipt_bytes)
                .expect("same receipt bytes");
            replacement
                .sync_all()
                .expect("sync fixture receipt replacement");
        } else {
            fs::create_dir(&target).expect("fixture replacement directory");
            fs::set_permissions(&target, fs::Permissions::from_mode(0o700))
                .expect("replacement has original safe directory mode");
        }
        self.facts.lock().expect("facts").final_substitution = Some(evidence);
        self.event("final-auth-input-substituted");
    }
    fn restore_final_auth_input(&self) -> TestResult<FinalAuthSubstitution> {
        let evidence = self
            .facts
            .lock()
            .expect("facts")
            .final_substitution
            .clone()
            .expect("actual substitution evidence");
        if evidence.kind == FinalAuthMutation::Receipt {
            fs::remove_file(&evidence.target)?;
        } else {
            // An unexpected write must fail this empty-directory removal;
            // do not recursively erase evidence of unauthorized activity.
            fs::remove_dir(&evidence.target)?;
        }
        fs::rename(&evidence.saved, &evidence.target)?;
        assert_restored_auth_identity(&evidence.target, &evidence.target_metadata)?;
        assert_final_auth_continuity(self, &evidence)?;
        Ok(evidence)
    }
}
fn assert_restored_auth_identity(path: &Path, original: &fs::Metadata) -> TestResult {
    use std::os::unix::fs::MetadataExt;
    let actual = fs::symlink_metadata(path)?;
    assert_eq!(
        (
            actual.dev(),
            actual.ino(),
            actual.uid(),
            actual.gid(),
            actual.mode(),
            actual.is_dir(),
            actual.is_file()
        ),
        (
            original.dev(),
            original.ino(),
            original.uid(),
            original.gid(),
            original.mode(),
            original.is_dir(),
            original.is_file()
        )
    );
    if actual.is_file() {
        assert_eq!(
            (actual.nlink(), actual.len()),
            (original.nlink(), original.len())
        );
    }
    Ok(())
}
fn assert_final_auth_continuity(
    observer: &AuthObserver,
    evidence: &FinalAuthSubstitution,
) -> TestResult {
    assert_restored_auth_identity(&observer.state_path(), &evidence.sdk_metadata)?;
    let controls = observer.auth_release_plan().controls;
    let mount = crate::data::startup_gate::revalidate_mount_source(&controls)?;
    let parent = mount
        .parent()
        .ok_or_else(|| io::Error::other("actual control parent missing"))?;
    assert_restored_auth_identity(&parent.join("launch.json"), &evidence.receipt_metadata)?;
    assert_eq!(
        fs::read(parent.join("launch.json"))?,
        evidence.receipt_bytes
    );
    assert_eq!(
        fs::read(parent.join("request.json"))?,
        controls.request.request_bytes()
    );
    assert_eq!(observer.activation(), evidence.intent);
    Ok(())
}
fn assert_no_final_mutation_permission(observer: &AuthObserver) -> TestResult {
    let evidence = observer
        .facts
        .lock()
        .expect("facts")
        .final_substitution
        .clone()
        .expect("actual substitution evidence");
    let mut directories = vec![observer.auth_control_path()];
    if evidence.kind == FinalAuthMutation::ControlChild {
        directories.push(evidence.saved);
    }
    for dir in directories {
        for name in ["release.json", ".released"] {
            assert_eq!(
                fs::symlink_metadata(dir.join(name))
                    .err()
                    .map(|error| error.kind()),
                Some(io::ErrorKind::NotFound),
                "mutation allowed permission/ACK or hid its absence"
            );
        }
    }
    let events = observer.facts.lock().expect("facts").events.clone();
    assert!(!events.contains(&"protected-release"));
    observer.assert_locked();
    observer.assert_unpublished();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replaced_auth_receipt_inode_rejects_before_take_and_preserves_original_custody(
) -> TestResult {
    use std::os::unix::fs::MetadataExt;
    let f = Fixture::new(&[])?;
    hold_real_auth_ack(&f)?;
    f.observer
        .cleanup_unknown_after_reap
        .store(true, Ordering::SeqCst);
    *f.observer.final_mutation.lock().expect("fixture mutation") = Some(FinalAuthMutation::Receipt);
    let (user, _output) = f.frontend();
    let exec = start(&f, user);
    let cancel = exec.cancel_handle();
    let waiter = Waiter::new(exec, Arc::clone(&f.observer));
    let AuthRetirement::Retained { ticket, error: _ } = waiter.finish().await else {
        panic!("replaced actual receipt must retain live child/original SDK custody");
    };
    assert!(f.probe.contains(ticket));
    assert!(f
        .observer
        .facts
        .lock()
        .expect("facts")
        .auth_release_takes
        .is_empty());
    assert_no_final_mutation_permission(&f.observer)?;
    let evidence = f
        .observer
        .facts
        .lock()
        .expect("facts")
        .final_substitution
        .clone()
        .expect("actual receipt substitution");
    let replacement = fs::symlink_metadata(&evidence.target)?;
    assert_ne!(
        (replacement.dev(), replacement.ino()),
        (
            evidence.receipt_metadata.dev(),
            evidence.receipt_metadata.ino()
        )
    );
    assert_eq!(replacement.permissions().mode() & 0o7777, 0o600);
    assert_eq!(fs::read(&evidence.target)?, evidence.receipt_bytes);
    let evidence = f.observer.restore_final_auth_input()?; // AFTER actual terminal rejection
    cancel.cancel()?;
    drop(cancel);
    f.reap_publication_failure(ticket)?;
    assert!(
        f.observer
            .facts
            .lock()
            .expect("facts")
            .auth_release_takes
            .is_empty(),
        "normal retained recovery reminted an auth attempt"
    );
    assert_no_final_mutation_permission(&f.observer)?;
    let preserved = f.keep_expected_publication_failure(ticket);
    preserved.assert_shutdown_retains(ticket);
    assert_final_auth_continuity(&preserved.observer, &evidence)?;
    Ok(())
}

async fn run_taken_final_auth_input_rejection(kind: FinalAuthMutation) -> TestResult {
    use std::os::unix::fs::MetadataExt;
    assert!(matches!(
        kind,
        FinalAuthMutation::SdkState | FinalAuthMutation::ControlChild
    ));
    let f = Fixture::new(&[])?;
    prepare_failed_release_replay(&f)?;
    *f.observer.final_mutation.lock().expect("fixture mutation") = Some(kind);
    let (user, _output) = f.frontend();
    let exec = start(&f, user);
    let cancel = exec.cancel_handle();
    let waiter = Waiter::new(exec, Arc::clone(&f.observer));
    f.observer.wait_event("failed-release-before-reattempt");
    // This records the ACTUAL first dispatch Err before namespace restoration.
    assert_eq!(
        f.observer
            .facts
            .lock()
            .expect("facts")
            .auth_release_attempt_errors,
        vec![true]
    );
    assert_eq!(
        f.observer.facts.lock().expect("facts").auth_release_takes,
        vec![true]
    );
    assert_no_final_mutation_permission(&f.observer)?;
    let evidence = f
        .observer
        .facts
        .lock()
        .expect("facts")
        .final_substitution
        .clone()
        .expect("actual final input substitution");
    let replacement = fs::symlink_metadata(&evidence.target)?;
    assert!(replacement.file_type().is_dir());
    assert_eq!(replacement.permissions().mode() & 0o7777, 0o700);
    assert_ne!(
        (replacement.dev(), replacement.ino()),
        (
            evidence.target_metadata.dev(),
            evidence.target_metadata.ino()
        )
    );
    assert_restored_auth_identity(&evidence.saved, &evidence.target_metadata)?;
    assert_eq!(f.observer.activation(), evidence.intent);
    let evidence = f.observer.restore_final_auth_input()?;
    // Even after the original names/pins return, the old launch cannot rearm.
    permit_failed_release_replay(&f)?;
    let AuthRetirement::Retained { ticket, error: _ } = waiter.finish().await else {
        panic!("actual final SDK/control rejection must retain original spent custody");
    };
    assert!(f.probe.contains(ticket));
    assert_no_final_mutation_permission(&f.observer)?;
    cancel.cancel()?;
    drop(cancel);
    f.reap_publication_failure(ticket)?;
    assert_eq!(
        f.observer
            .facts
            .lock()
            .expect("facts")
            .auth_release_attempt_errors,
        vec![true]
    );
    assert_no_final_mutation_permission(&f.observer)?;
    let preserved = f.keep_expected_publication_failure(ticket);
    preserved.assert_shutdown_retains(ticket);
    assert_final_auth_continuity(&preserved.observer, &evidence)?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exchanged_named_sdk_state_rejects_auth_and_spent_attempt_cannot_rearm() -> TestResult {
    run_taken_final_auth_input_rejection(FinalAuthMutation::SdkState).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exchanged_guest_control_child_rejects_auth_and_spent_attempt_cannot_rearm() -> TestResult {
    run_taken_final_auth_input_rejection(FinalAuthMutation::ControlChild).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_clean_cancellation_is_retryable_while_cancel_wrapper_is_paused() -> TestResult {
    struct CancelCaller {
        observer: Arc<AuthObserver>,
        thread: Option<std::thread::JoinHandle<Result<(), EngineError>>>,
    }
    impl CancelCaller {
        fn is_finished(&self) -> bool {
            self.thread.as_ref().expect("cancel thread").is_finished()
        }
        fn finish(mut self) -> Result<(), EngineError> {
            self.observer.cancel_return.release();
            self.thread
                .take()
                .expect("cancel thread")
                .join()
                .expect("actual cancellation thread panicked")
        }
    }
    impl Drop for CancelCaller {
        fn drop(&mut self) {
            self.observer.cancel_return.release();
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    let f = Fixture::new(&[])?;
    let (user, _output) = f.frontend();
    f.observer.hold_cleanup.store(true, Ordering::SeqCst);
    f.observer.hold_cancel_return.store(true, Ordering::SeqCst);
    let exec = start(&f, user);
    f.observer.wait_event("protected-release");
    let activation = f.observer.activation();
    let cancel = exec.cancel_handle();
    let waiter = Waiter::new(exec, Arc::clone(&f.observer));
    let caller = CancelCaller {
        observer: Arc::clone(&f.observer),
        thread: Some(std::thread::spawn(move || cancel.cancel())),
    };
    f.observer.cancel_return.wait_entered();
    f.observer.cleanup.wait_entered();
    assert!(!waiter.is_finished());
    assert!(!caller.is_finished());
    f.observer.assert_locked();
    f.observer.assert_unpublished();
    f.observer.cleanup.release();

    let retirement = waiter.finish().await;
    assert!(!caller.is_finished());
    let AuthRetirement::Retryable(retry) = retirement else {
        panic!("actual clean cancellation must be Retryable while its wrapper remains paused");
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
    let cancel_result = caller.finish();
    assert!(
        matches!(cancel_result, Ok(())),
        "actual delegated cancellation failed: {cancel_result:?}"
    );
    drop(retry);
    f.observer.assert_unlocked();
    Ok(())
}
