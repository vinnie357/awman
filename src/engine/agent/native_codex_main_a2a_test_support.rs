//! A2r observes actual retirement and strict completion; materials belong to A2m.
use super::native_codex_auth_preparation_test::{AuthObserver, TestResult};
use super::native_codex_main_test_support::{MainBoundary, MainFixture};
use super::AgentEngine;
use crate::engine::container::gated_launch::*;
use crate::engine::container::orchestrated_apple::*;
use crate::engine::container::ContainerRuntime;
use crate::engine::error::EngineError;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex};
use tokio::sync::oneshot;

pub(crate) fn is_main(argv: &[String]) -> bool {
    argv.iter()
        .position(|arg| arg == "--")
        .is_some_and(|index| {
            argv.get(index + 1).map(String::as_str) == Some("codex")
                && argv.get(index + 2).map(String::as_str) == Some("exec")
        })
}
#[derive(Clone)]
pub(crate) struct Snapshot {
    pub argv: Vec<String>,
    pub name: String,
    pub controls: PathBuf,
    pub active: Value,
    pub owner: Vec<u8>,
    pub ack: Option<Value>,
    pub release: Option<Value>,
    pub reaped: bool,
    pub cleaned: bool,
    pub released: bool,
}
#[derive(Default)]
struct Hold {
    started: Mutex<Option<(String, Vec<String>)>>,
    released: Mutex<bool>,
    changed: Condvar,
    entered: Mutex<Option<oneshot::Sender<()>>>,
}
impl Hold {
    fn release(&self) {
        *self.released.lock().unwrap_or_else(|p| p.into_inner()) = true;
        self.changed.notify_all();
    }
}
pub(crate) struct ReleaseGuard(Arc<Hold>);
impl ReleaseGuard {
    pub(crate) fn release(&self) {
        self.0.release();
    }
}
impl Drop for ReleaseGuard {
    fn drop(&mut self) {
        self.0.release();
    }
}
struct ResourceInstrument {
    hold: Arc<Hold>,
    name: String,
    argv: Vec<String>,
}
impl Drop for ResourceInstrument {
    fn drop(&mut self) {
        let started = self
            .hold
            .started
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .is_some_and(|(name, argv)| name == &self.name && argv == &self.argv);
        if !started {
            return;
        }
        if let Some(sender) = self
            .hold
            .entered
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        {
            let _ = sender.send(());
        }
        let mut released = self.hold.released.lock().unwrap_or_else(|p| p.into_inner());
        while !*released {
            released = self
                .hold
                .changed
                .wait(released)
                .unwrap_or_else(|p| p.into_inner());
        }
    }
}
pub(crate) struct ObservedBoundary {
    inner: Arc<MainBoundary>,
    observer: Arc<AuthObserver>,
    hold: Arc<Hold>,
    pub snapshots: Mutex<Vec<Snapshot>>,
}
impl ObservedBoundary {
    fn current(&self, update: impl FnOnce(&mut Snapshot)) {
        if let Some(snapshot) = self.snapshots.lock().expect("snapshots").last_mut() {
            update(snapshot);
        }
    }
}
pub(crate) struct A2Fixture {
    pub fixture: MainFixture,
    pub boundary: Arc<ObservedBoundary>,
    settled: bool,
}
impl A2Fixture {
    pub(crate) fn new(main: bool) -> TestResult<(Self, ReleaseGuard, oneshot::Receiver<()>)> {
        let mut fixture = MainFixture::new()?;
        let (sender, entered) = oneshot::channel();
        let hold = Arc::new(Hold::default());
        *hold.entered.lock().expect("entry sender") = Some(sender);
        let boundary = Arc::new(ObservedBoundary {
            inner: Arc::clone(&fixture.boundary),
            observer: Arc::clone(&fixture.auth.observer),
            hold: Arc::clone(&hold),
            snapshots: Mutex::new(Vec::new()),
        });
        fixture.auth.runtime =
            Arc::new(ContainerRuntime::apple_with_test_boundary(boundary.clone()));
        let engine = AgentEngine::new(
            Arc::clone(&fixture.auth.overlay),
            Arc::clone(&fixture.auth.runtime),
        )
        .with_native_codex_auth_test_observer(Arc::clone(&fixture.auth.observer));
        let engine = if main {
            engine.with_native_codex_main_test_opt_in()
        } else {
            engine
        };
        fixture.auth.engine = engine;
        Ok((
            Self {
                fixture,
                boundary,
                settled: false,
            },
            ReleaseGuard(hold),
            entered,
        ))
    }
    pub(crate) fn settled(&mut self) {
        self.settled = true;
    }
}
impl Drop for A2Fixture {
    fn drop(&mut self) {
        self.boundary.hold.release();
        if !self.settled {
            self.fixture
                .auth
                .observer
                .teardown_uncertain
                .store(true, Ordering::SeqCst);
        }
    }
}
impl OrchestratedAppleTestBoundary for ObservedBoundary {
    fn take_launch_resource(
        &self,
        name: &str,
        argv: &[String],
    ) -> Option<Box<dyn std::any::Any + Send>> {
        if !is_main(argv) {
            return None;
        }
        let delimiter = argv
            .iter()
            .position(|arg| arg == "--")
            .expect("command delimiter");
        let names = argv[..delimiter]
            .windows(2)
            .filter(|pair| pair[0] == "--name")
            .map(|pair| pair[1].as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [name],
            "actual container name differs from protected plan"
        );
        Some(Box::new(ResourceInstrument {
            hold: Arc::clone(&self.hold),
            name: name.to_owned(),
            argv: argv.to_vec(),
        }))
    }
    fn observe_spawn(
        &self,
        argv: Vec<String>,
        name: String,
        labels: Vec<String>,
        mounts: Vec<(String, bool)>,
        registry: bool,
        request: Vec<u8>,
        manifests: BTreeMap<String, Vec<u8>>,
    ) {
        self.observer.assert_locked();
        let controls = argv
            .windows(2)
            .find_map(|pair| {
                (pair[0] == "-v" || pair[0] == "--volume")
                    .then(|| pair[1].strip_suffix(":/.awman/startup-gate/control"))
                    .flatten()
                    .map(PathBuf::from)
            })
            .expect("actual controls mount");
        let state = self.observer.state_path();
        self.snapshots.lock().expect("snapshots").push(Snapshot {
            argv: argv.clone(),
            name: name.clone(),
            controls,
            active: self.observer.activation(),
            owner: fs::read(state.parent().expect("owner").join("owner.json"))
                .expect("owner bytes"),
            ack: None,
            release: None,
            reaped: false,
            cleaned: false,
            released: false,
        });
        self.inner
            .observe_spawn(argv, name, labels, mounts, registry, request, manifests);
    }
    fn observe_child_start(&self) {
        self.current(|snapshot| {
            if is_main(&snapshot.argv) {
                let mut started = self.hold.started.lock().expect("actual start");
                assert!(started.is_none(), "extra actual main start");
                *started = Some((snapshot.name.clone(), snapshot.argv.clone()));
            }
        });
        self.inner.observe_child_start();
    }
    fn begin_cleanup(&self) {
        self.current(|s| {
            s.ack = fs::read(s.controls.join(".released"))
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok());
            s.release = fs::read(s.controls.join("release.json"))
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok());
        });
        self.inner.begin_cleanup();
    }
    fn observe_actual_child_reaped(&self) {
        self.current(|s| s.reaped = true);
        self.inner.observe_actual_child_reaped();
    }
    fn observe_cleanup_publication(&self, succeeded: bool) {
        self.current(|s| s.cleaned = succeeded);
        self.inner.observe_cleanup_publication(succeeded);
    }
    fn observe_resource_loan_released(&self) {
        self.current(|s| s.released = true);
        self.inner.observe_resource_loan_released();
    }
    fn retention_available(&self) -> bool {
        self.inner.retention_available()
    }
    fn image_inspection(&self) -> Result<Vec<u8>, EngineError> {
        self.inner.image_inspection()
    }
    fn provider(&self) -> Arc<dyn GatedProviderAdapter> {
        self.inner.provider()
    }
    fn executable(&self) -> &'static str {
        self.inner.executable()
    }
    fn prepare_simulated_guest(
        &self,
        plan: &DurableLaunchPlan,
    ) -> Result<(), LaunchRetentionReason> {
        self.inner.prepare_simulated_guest(plan)
    }
    fn pause_before_ready(&self) -> bool {
        self.inner.pause_before_ready()
    }
    fn pause_after_ready(&self) -> bool {
        self.inner.pause_after_ready()
    }
    fn observe_protected_launch_identity(&self, inspection: &ProviderLaunchInspection) {
        self.inner.observe_protected_launch_identity(inspection)
    }
    fn observe_ready_rejected(&self) {
        self.inner.observe_ready_rejected()
    }
    fn observe_protected_ready(&self) {
        self.inner.observe_protected_ready()
    }
    fn observe_accepted_release(&self) {
        self.inner.observe_accepted_release()
    }
    fn observe_provider_entry(&self, before: bool) {
        self.inner.observe_provider_entry(before)
    }
    fn observe_cancel_point(&self, point: TestCancellationPoint) {
        self.inner.observe_cancel_point(point)
    }
    fn before_cli_start_after_plan(&self) -> bool {
        self.inner.before_cli_start_after_plan()
    }
    fn observe_reap(&self) {
        self.inner.observe_reap()
    }
    fn observe_never_created_proven(&self) {
        self.inner.observe_never_created_proven()
    }
    fn observe_exact_absence_proven(&self) {
        self.inner.observe_exact_absence_proven()
    }
    fn fail_cleanup_publication_once(&self) -> bool {
        self.inner.fail_cleanup_publication_once()
    }
    fn cleanup_reap_proven(&self) -> bool {
        self.inner.cleanup_reap_proven()
    }
    fn observe_custody(&self, ownership: TestOwnershipObservation, legacy: bool) {
        self.inner.observe_custody(ownership, legacy)
    }
}

pub(crate) fn assert_held_retirement(f: &A2Fixture) -> TestResult {
    let snapshots = f.boundary.snapshots.lock().expect("snapshots");
    assert_eq!(snapshots.len(), 2);
    let (auth, main) = (&snapshots[0], &snapshots[1]);
    assert!(!is_main(&auth.argv) && is_main(&main.argv));
    assert!(auth.reaped && auth.cleaned && auth.released);
    assert!(main.reaped && main.cleaned && !main.released);
    assert_eq!(main.active, auth.active);
    assert_eq!(main.owner, auth.owner);
    assert_eq!(main.active, f.fixture.auth.observer.activation());
    f.fixture.auth.observer.assert_locked();
    f.fixture.auth.observer.assert_unpublished();
    assert!(main.ack.is_some());
    assert_eq!(main.ack, main.release);
    Ok(())
}
pub(crate) fn assert_completed(f: &A2Fixture) -> TestResult {
    let snapshots = f.boundary.snapshots.lock().expect("snapshots");
    assert!(snapshots[1].released);
    let active = &snapshots[0].active;
    let state = f.fixture.auth.observer.state_path();
    let completed: Value = serde_json::from_slice(&fs::read(
        state.parent().ok_or("owner")?.join("completed-run.json"),
    )?)?;
    assert_eq!(completed["run_id"], active["run_id"]);
    assert_eq!(completed["owner"], active["owner"]);
    assert_eq!(completed["requested_session"], active["requested_session"]);
    assert_eq!(completed["session_id"], "connected-main-fixture");
    assert_eq!(f.fixture.auth.observer.activation(), *active);
    f.fixture.auth.observer.assert_unlocked();
    Ok(())
}
