//! A1 connected first-main witness; only the external CLI/provider is substituted.
use super::*;
use crate::command::commands::agent_auth::AgentAuthDecision;
use crate::command::commands::agent_setup::AgentSetupDecision;
use crate::command::commands::mount_scope::MountScopeDecision;
use crate::data::fs::native_codex_auth::GUEST_CODEX_HOME as SDK;
use crate::engine::agent::native_codex_auth_preparation_test::{
    AuthObserver, Fixture, TestResult, UserOutput, QUESTION,
};
use crate::engine::agent::AgentEngine;
use crate::engine::agent_runtime::AgentFrontend;
use crate::engine::container::gated_launch::*;
use crate::engine::container::orchestrated_apple::{
    OrchestratedAppleTestBoundary, TestCancellationPoint, TestOwnershipObservation,
};
use crate::engine::container::ContainerRuntime;
use crate::engine::error::EngineError;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const SESSION: &str = "synthetic-connected-thread";
const BIN: &str = "/.awman/startup-gate/bin";
const CONTROL: &str = "/.awman/startup-gate/control";
const MARKER: &[u8] = b"actual-auth-child-mutation\n";
const WATCHDOG: Duration = Duration::from_secs(15);
const RETIREMENT: [&str; 5] = [
    "actual-reap",
    "exact-remove",
    "exact-absence",
    "cleanup-published",
    "resources-released",
];

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

struct Launch {
    argv: Vec<String>,
    request: Value,
    manifests: BTreeMap<String, Vec<u8>>,
    mounts: BTreeMap<String, (PathBuf, bool)>,
    physical: (u64, u64),
    activation: Value,
    plan: Option<DurableLaunchPlan>,
    event_start: usize,
    events: Vec<&'static str>,
}
struct Boundary {
    observer: Arc<AuthObserver>,
    launches: Mutex<Vec<Launch>>,
    executable: &'static str,
}
impl Boundary {
    fn snapshot_events(&self) {
        let facts = self.observer.facts.lock().expect("facts");
        if let Some(launch) = self.launches.lock().expect("launches").last_mut() {
            launch.events = facts.events[launch.event_start..].to_vec();
        }
    }
    fn assert_auth_retired(&self) {
        let launches = self.launches.lock().expect("launches");
        let auth = launches.first().expect("actual auth launch");
        for event in RETIREMENT {
            assert!(auth.events.contains(&event), "auth missing actual {event}");
        }
    }
}
impl OrchestratedAppleTestBoundary for Arc<Boundary> {
    fn retention_available(&self) -> bool {
        self.observer.retention_available()
    }
    fn image_inspection(&self) -> Result<Vec<u8>, EngineError> {
        self.observer.image_inspection()
    }
    fn provider(&self) -> Arc<dyn GatedProviderAdapter> {
        Arc::clone(&self.observer) as Arc<dyn GatedProviderAdapter>
    }
    fn executable(&self) -> &'static str {
        self.executable
    }
    fn prepare_simulated_guest(
        &self,
        plan: &DurableLaunchPlan,
    ) -> Result<(), LaunchRetentionReason> {
        let name = plan.key.container_name.as_str();
        let mut launches = self.launches.lock().expect("launches");
        let launch = launches
            .iter_mut()
            .find(|launch| launch.argv.windows(2).any(|pair| pair == ["--name", name]))
            .expect("actual launch identity");
        let controls = &plan.controls;
        let source =
            crate::data::startup_gate::revalidate_mount_source(controls).expect("live controls");
        assert_eq!(
            launch.mounts[CONTROL].0,
            fs::canonicalize(source).expect("live source")
        );
        assert_eq!(
            fs::read(launch.mounts[BIN].0.join("request.json")).expect("live request"),
            controls.request.request_bytes()
        );
        launch.plan = Some(plan.clone());
        drop(launches);
        self.observer.prepare_simulated_guest(plan)
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
        labels: Vec<String>,
        writable: Vec<(String, bool)>,
        registry: bool,
        request: Vec<u8>,
        manifests: BTreeMap<String, Vec<u8>>,
    ) {
        self.snapshot_events();
        if argv.windows(2).any(|pair| pair == ["codex", "exec"]) {
            self.assert_auth_retired();
            self.observer.assert_unpublished();
        }
        assert!(!argv.iter().any(|arg| {
            arg.starts_with("--mount")
                || arg.starts_with("--volume=")
                || (arg.starts_with("-v") && arg != "-v")
        }));
        let mut mounts = BTreeMap::new();
        for pair in argv
            .windows(2)
            .filter(|pair| pair[0] == "-v" || pair[0] == "--volume")
        {
            let parts = pair[1].split(':').collect::<Vec<_>>();
            assert!((2..=3).contains(&parts.len()));
            assert!(parts.len() == 2 || matches!(parts[2], "ro" | "rw"));
            let source = fs::canonicalize(parts[0]).expect("canonical mount source");
            let read_only = parts.get(2) == Some(&"ro");
            assert!(mounts
                .insert(parts[1].to_owned(), (source, read_only))
                .is_none());
        }
        assert_eq!(
            argv.iter()
                .filter(|arg| *arg == "-v" || *arg == "--volume")
                .count(),
            mounts.len()
        );
        let sdk = &mounts[SDK].0;
        assert_eq!(fs::canonicalize(sdk).expect("canonical SDK"), *sdk);
        let metadata = fs::metadata(sdk).expect("actual SDK identity");
        let event_start = self.observer.facts.lock().expect("facts").events.len();
        let activation = self.observer.activation();
        self.launches.lock().expect("launches").push(Launch {
            argv: argv.clone(),
            request: serde_json::from_slice(&request).expect("actual request"),
            manifests: manifests.clone(),
            mounts,
            physical: (metadata.dev(), metadata.ino()),
            activation,
            plan: None,
            event_start,
            events: Vec::new(),
        });
        self.observer
            .observe_spawn(argv, name, labels, writable, registry, request, manifests);
    }
    fn observe_child_start(&self) {
        self.observer.observe_child_start();
    }
    fn observe_protected_launch_identity(&self, inspection: &ProviderLaunchInspection) {
        self.observer.observe_protected_launch_identity(inspection);
    }
    fn observe_ready_rejected(&self) {
        self.observer.observe_ready_rejected();
    }
    fn observe_protected_ready(&self) {
        self.observer.observe_protected_ready();
    }
    fn observe_accepted_release(&self) {
        self.observer.observe_accepted_release();
    }
    fn observe_provider_entry(&self, before: bool) {
        self.observer.observe_provider_entry(before);
    }
    fn observe_cancel_point(&self, point: TestCancellationPoint) {
        self.observer.observe_cancel_point(point);
    }
    fn begin_cleanup(&self) {
        self.observer.begin_cleanup();
    }
    fn observe_reap(&self) {
        self.observer.observe_reap();
    }
    fn observe_actual_child_reaped(&self) {
        self.observer.observe_actual_child_reaped();
    }
    fn observe_never_created_proven(&self) {
        self.observer.observe_never_created_proven();
    }
    fn observe_exact_absence_proven(&self) {
        self.observer.observe_exact_absence_proven();
    }
    fn observe_cleanup_publication(&self, succeeded: bool) {
        self.observer.observe_cleanup_publication(succeeded);
    }
    fn observe_resource_loan_released(&self) {
        self.observer.observe_resource_loan_released();
    }
    fn cleanup_reap_proven(&self) -> bool {
        self.observer.cleanup_reap_proven()
    }
    fn observe_custody(&self, facts: TestOwnershipObservation, legacy: bool) {
        self.observer.observe_custody(facts, legacy);
    }
}
struct Frontend(VecDeque<Box<dyn AgentFrontend>>);
impl UserMessageSink for Frontend {
    fn write_message(&mut self, _: UserMessage) {}
    fn replay_queued(&mut self) {}
}
impl crate::command::commands::agent_setup::HasAgentFrontend for Frontend {
    fn container_frontend(&mut self) -> Box<dyn AgentFrontend> {
        self.0.pop_front().expect("one stream per actual launch")
    }
}
impl AgentSetupFrontend for Frontend {
    fn ask_agent_setup(
        &mut self,
        _: &AgentName,
        _: &AgentName,
        _: bool,
        _: bool,
    ) -> Result<AgentSetupDecision, CommandError> {
        panic!("generic setup reached")
    }
    fn record_fallback(&mut self, _: &AgentName, _: &AgentName) {
        panic!("generic fallback reached")
    }
}
impl AgentAuthFrontend for Frontend {
    fn ask_agent_auth_consent(
        &mut self,
        _: &AgentName,
        _: &[&str],
    ) -> Result<AgentAuthDecision, CommandError> {
        panic!("generic host auth reached")
    }
}
impl MountScopeFrontend for Frontend {
    fn ask_mount_scope(&mut self, _: &Path, _: &Path) -> Result<MountScopeDecision, CommandError> {
        panic!("scope unexpectedly widened")
    }
}
impl crate::engine::acp::AcpFrontend for Frontend {
    fn render_update(&mut self, _: crate::engine::acp::SessionUpdate) {
        panic!("ACP reached")
    }
    fn request_permission(
        &mut self,
        _: crate::engine::acp::PermissionRequest,
    ) -> crate::engine::acp::PermissionDecision {
        panic!("ACP reached")
    }
    fn next_prompt(&mut self) -> Option<String> {
        panic!("extra question requested")
    }
}
#[async_trait]
impl ExecPromptCommandFrontend for Frontend {}
struct Run {
    result: tokio::sync::oneshot::Receiver<Result<ExecPromptOutcome, CommandError>>,
    thread: Option<std::thread::JoinHandle<()>>,
    runtime: Arc<ContainerRuntime>,
    observer: Arc<AuthObserver>,
    root: PathBuf,
    outputs: [UserOutput; 2],
}
impl Run {
    async fn finish(&mut self) -> TestResult<Result<ExecPromptOutcome, CommandError>> {
        let result = tokio::time::timeout(WATCHDOG, &mut self.result).await??;
        self.thread
            .take()
            .expect("thread")
            .join()
            .expect("command thread panicked");
        Ok(result)
    }
}
impl Drop for Run {
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = fs::write(self.root.join("fixture-stop"), b"stop");
            let _ = thread.join();
        }
        let registry = self
            .runtime
            .launch_retention()
            .expect("actual command registry");
        let state = retention_test_support::registry_probe(&registry).snapshot();
        if state.retained != 0 || state.unreaped != 0 {
            self.observer
                .teardown_uncertain
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
}
fn boundary(f: &Fixture) -> TestResult<Arc<Boundary>> {
    let executable = f.path().join("connected-container-fixture");
    let root = f.path().to_string_lossy().replace('\'', "'\\''");
    fs::write(
        &executable,
        format!(
            r##"#!/bin/sh
root='{root}'
control='' sdk='' mode='' prev=''
for arg in "$@"; do
  if [ "$prev" = '-v' ] || [ "$prev" = '--volume' ]; then
    case "$arg" in
      *:/.awman/startup-gate/control) control=${{arg%:/.awman/startup-gate/control}} ;;
      *:/data/awman-native-sdk/codex) sdk=${{arg%:/data/awman-native-sdk/codex}} ;;
    esac
  fi
  [ "$arg" != 'login' ] || mode=auth
  [ "$arg" != 'exec' ] || mode=main
  prev=$arg
done
[ -n "$control" ] && [ -n "$sdk" ] || exit 91
while [ ! -f "$control/release.json" ]; do
  [ ! -f "$root/fixture-stop" ] || exit 92
  sleep 0.01
done
umask 077
printf '%s' '{{"nonce":"2222222222222222222222222222222222222222222222222222222222222222","version":1}}' > "$control/.released.tmp.$$"
mv "$control/.released.tmp.$$" "$control/.released" || exit 93
if [ "$mode" = auth ]; then
  printf 'actual-auth-child-mutation\n' > "$sdk/post-auth.marker"
  printf '%s\n' 'https://device.invalid/SYNTHETIC_DEVICE_CHALLENGE'
elif [ "$mode" = main ]; then
  exec 3<&0
  cat <&3 > "$root/actual-main-stdin" &
  reader=$!
  exec 3<&-
  (
    while kill -0 "$reader" 2>/dev/null; do
      if [ -f "$root/fixture-stop" ]; then
        kill -TERM "$reader" 2>/dev/null
        exit
      fi
      sleep 0.01
    done
  ) &
  watcher=$!
  wait "$reader"
  status=$?
  kill -TERM "$watcher" 2>/dev/null
  wait "$watcher" 2>/dev/null
  [ "$status" -eq 0 ] && [ ! -f "$root/fixture-stop" ] || exit 95
  printf '%s\n' '{{"type":"thread.started","thread_id":"{SESSION}"}}' '{{"type":"turn.started"}}' '{{"type":"turn.completed"}}'
else exit 94
fi
"##
        ),
    )?;
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700))?;
    let executable = Box::leak(executable.to_string_lossy().into_owned().into_boxed_str());
    Ok(Arc::new(Boundary {
        observer: Arc::clone(&f.observer),
        launches: Mutex::new(Vec::new()),
        executable,
    }))
}
fn start(f: &Fixture, boundary: &Arc<Boundary>) -> Run {
    let runtime = Arc::new(ContainerRuntime::apple_with_test_boundary(Arc::new(
        Arc::clone(boundary),
    )));
    let engine = AgentEngine::new(Arc::clone(&f.overlay), Arc::clone(&runtime))
        .with_native_codex_auth_test_observer(Arc::clone(&f.observer))
        .with_native_codex_main_test_observer(Arc::clone(&f.observer));
    let mut command = super::native_codex_auth_entry_test::command(f);
    command.engines.runtime = runtime.clone();
    command.engines.container_runtime = Some(Arc::clone(&runtime));
    command.engines.agent_engine = Arc::new(engine);
    let (auth, auth_output) = f.frontend();
    let (main, main_output) = f.frontend();
    let frontend = Frontend([auth, main].into_iter().collect());
    let (sender, result) = tokio::sync::oneshot::channel();
    let thread = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("fixture runtime");
        let _ = sender.send(runtime.block_on(command.run_with_frontend(Box::new(frontend))));
    });
    Run {
        result,
        thread: Some(thread),
        runtime,
        observer: Arc::clone(&f.observer),
        root: f.path().to_owned(),
        outputs: [auth_output, main_output],
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fresh_command_hands_retired_auth_once_to_main_on_the_original_lease() -> TestResult {
    let f = Fixture::new(&[])?;
    let boundary = boundary(&f)?;
    let mut run = start(&f, &boundary);
    let result = run.finish().await?;
    boundary.snapshot_events();
    boundary.assert_auth_retired();
    if result.is_err() {
        assert!(
            matches!(&result, Err(CommandError::Engine(EngineError::Config(reason)))
                if reason == "native Codex main execution remains unqualified"),
            "unexpected early failure: {result:?}"
        );
    }
    assert!(
        result.is_ok(),
        "connected command stopped at closed main after actual clean auth: {result:?}"
    );
    assert_eq!(
        result.as_ref().expect("completed command").exit_code,
        Some(0)
    );
    let sdk = f.observer.state_path();
    let owner = sdk.parent().expect("owner").to_owned();
    let bytes = run.outputs[1].bytes();
    let expected = format!(
        "{{\"type\":\"thread.started\",\"thread_id\":\"{SESSION}\"}}\n{{\"type\":\"turn.started\"}}\n{{\"type\":\"turn.completed\"}}\n"
    );
    assert_eq!(bytes, expected.as_bytes(), "exact combined fixture output");
    let launches = boundary.launches.lock().expect("launches");
    assert_eq!(launches.len(), 2, "one auth plus one main, without replay");
    let (auth, main) = (&launches[0], &launches[1]);
    let auth_argv = ["codex".into(), "login".into(), "--device-auth".into()];
    assert!(auth.argv.ends_with(&auth_argv));
    assert!(!auth.argv.iter().any(|arg| arg.contains(QUESTION)));
    assert_eq!(auth.physical, main.physical);
    assert_eq!(auth.activation, main.activation);
    assert_eq!(auth.mounts[SDK], main.mounts[SDK]);
    assert_eq!(
        main.mounts["/workspace"].0,
        fs::canonicalize(f.session.git_root())?
    );
    for (launch, is_main) in [(auth, false), (main, true)] {
        assert_eq!(launch.mounts.len(), if is_main { 4 } else { 3 });
        for (guest, (source, read_only)) in &launch.mounts {
            let expected = match guest.as_str() {
                SDK | CONTROL => false,
                BIN => true,
                "/workspace" if is_main => true,
                _ => panic!("unexpected guest mount: {guest}"),
            };
            assert_eq!(*read_only, expected);
            if guest != SDK {
                assert!(!source.starts_with(&owner) && !owner.starts_with(source));
            }
        }
    }
    assert!(!main.argv.iter().any(|arg| arg.contains(QUESTION)));
    assert_eq!(
        fs::read(f.path().join("actual-main-stdin"))?,
        QUESTION.as_bytes()
    );
    assert_eq!(
        fs::read(f.observer.state_path().join("post-auth.marker"))?,
        MARKER
    );
    assert!(!main
        .argv
        .iter()
        .any(|arg| { arg == "--output-last-message" || arg.contains("/.codex") }));
    let a = auth.plan.as_ref().expect("auth plan");
    let m = main.plan.as_ref().expect("main plan");
    assert_ne!(a.key.container_name, m.key.container_name);
    assert_ne!(a.key.token_digest, m.key.token_digest);
    assert!(!Arc::ptr_eq(&a.controls, &m.controls));
    assert_ne!(auth.request, main.request);
    let bindings = main.request["bindings"].as_array().expect("main bindings");
    assert_eq!(bindings.len(), 2);
    for (guest, access) in [(SDK, "read-write"), ("/workspace", "read-only")] {
        let count = bindings
            .iter()
            .filter(|binding| binding["workspace_path"] == guest && binding["access"] == access)
            .count();
        assert_eq!(count, 1);
    }
    let digest = hex(&Sha256::digest(MARKER));
    assert!(main.manifests.values().any(|bytes| {
        let manifest: Value = serde_json::from_slice(bytes).expect("actual manifest");
        manifest["entries"]
            .as_array()
            .expect("entries")
            .iter()
            .any(|entry| {
                entry["path"] == "post-auth.marker"
                    && entry["sha256"] == digest
                    && entry["size"] == MARKER.len()
            })
    }));
    let gate = [
        "child-start",
        "exact-ready",
        "protected-ready",
        "protected-release",
    ];
    for event in RETIREMENT.into_iter().chain(gate) {
        assert!(main.events.contains(&event), "main missing actual {event}");
    }
    let completed: Value = serde_json::from_slice(&fs::read(owner.join("completed-run.json"))?)?;
    assert_eq!(completed["owner"], auth.activation["owner"]);
    assert_eq!(completed["run_id"], auth.activation["run_id"]);
    assert_eq!(completed["requested_session"], Value::Null);
    assert_eq!(completed["session_id"], SESSION);
    assert_eq!(f.observer.activation(), auth.activation);
    let session_name = format!("{}.json", hex(&Sha256::digest(SESSION.as_bytes())));
    let sessions = owner
        .parent()
        .expect("owners")
        .parent()
        .expect("key")
        .join("sessions");
    let association: Value = serde_json::from_slice(&fs::read(sessions.join(session_name))?)?;
    assert_eq!(association["owner"], completed["owner"]);
    assert_eq!(association["session_id"], SESSION);
    let registry = run
        .runtime
        .launch_retention()
        .expect("actual command registry");
    let state = retention_test_support::registry_probe(&registry).snapshot();
    assert_eq!((state.retained, state.unreaped), (0, 0));
    f.observer.assert_unlocked();
    Ok(())
}
