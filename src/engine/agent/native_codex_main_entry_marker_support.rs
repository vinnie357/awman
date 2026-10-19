//! Test-only actual-child marker and named pre-CLI physical-exchange insertion point.
use super::native_codex_auth_preparation_test::TestResult;
use super::native_codex_main_a2a_test_support::{is_main, A2Fixture};
use super::native_codex_main_a2m_test_support::Materials;
use super::AgentEngine;
use crate::engine::container::gated_launch::*;
use crate::engine::container::orchestrated_apple::*;
use crate::engine::container::ContainerRuntime;
use crate::engine::error::EngineError;
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

type BeforeMainCli = Box<dyn FnOnce(&[String]) + Send>;

pub(crate) struct EntryBoundary {
    inner: Arc<Materials>,
    before_main_cli: Mutex<Option<BeforeMainCli>>,
    main_phase: AtomicBool,
    pub marker_dir: PathBuf,
    pub main_started: AtomicBool,
    pub main_reaped: AtomicBool,
    pub main_never_created: AtomicBool,
}
pub(crate) fn install(
    f: &mut A2Fixture,
    scope: Arc<Materials>,
    before_main_cli: BeforeMainCli,
) -> TestResult<Arc<EntryBoundary>> {
    let marker_dir = f.fixture.auth.path().join("fixture-entry-markers");
    fs::create_dir(&marker_dir)?;
    let marker = serde_json::to_string(&marker_dir.join("main.json"))?;
    let executable = scope.executable();
    let script = fs::read_to_string(executable)?;
    let anchor = "\nelse:\n    print('{\"type\":\"thread.started\",\"thread_id\":\"connected-main-fixture\"}', flush=True)\n";
    assert_eq!(
        script.matches(anchor).count(),
        1,
        "exact main branch anchor"
    );
    let replacement = format!(
        "\nelse:\n    names = [args[i + 1] for i, arg in enumerate(args[:-1]) if arg == '--name']\n    assert len(names) == 1\n    with pathlib.Path({marker}).open('x') as entry:\n        json.dump(dict(phase='main', pid=os.getpid(), container_name=names[0]), entry)\n    print('{{\"type\":\"thread.started\",\"thread_id\":\"connected-main-fixture\"}}', flush=True)\n"
    );
    fs::write(executable, script.replacen(anchor, &replacement, 1))?;
    let outer = Arc::new(EntryBoundary {
        inner: scope,
        before_main_cli: Mutex::new(Some(before_main_cli)),
        main_phase: AtomicBool::new(false),
        marker_dir,
        main_started: AtomicBool::new(false),
        main_reaped: AtomicBool::new(false),
        main_never_created: AtomicBool::new(false),
    });
    f.fixture.auth.runtime = Arc::new(ContainerRuntime::apple_with_test_boundary(outer.clone()));
    f.fixture.auth.engine = AgentEngine::new(
        Arc::clone(&f.fixture.auth.overlay),
        Arc::clone(&f.fixture.auth.runtime),
    )
    .with_native_codex_auth_test_observer(Arc::clone(&f.fixture.auth.observer))
    .with_native_codex_main_test_opt_in();
    Ok(outer)
}
impl OrchestratedAppleTestBoundary for EntryBoundary {
    fn take_launch_resource(
        &self,
        name: &str,
        argv: &[String],
    ) -> Option<Box<dyn std::any::Any + Send>> {
        self.inner.take_launch_resource(name, argv)
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
        let main = is_main(&argv);
        self.main_phase.store(main, Ordering::Release);
        let actual_argv = argv.clone();
        self.inner
            .observe_spawn(argv, name, labels, mounts, registry, request, manifests);
        if main {
            self.before_main_cli
                .lock()
                .expect("main insertion point")
                .take()
                .expect("one main before-CLI observer")(&actual_argv);
        }
    }
    fn observe_resource_loan_released(&self) {
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
    fn observe_child_start(&self) {
        if self.main_phase.load(Ordering::Acquire) {
            self.main_started.store(true, Ordering::Release);
        }
        self.inner.observe_child_start()
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
    fn begin_cleanup(&self) {
        self.inner.begin_cleanup()
    }
    fn observe_reap(&self) {
        self.inner.observe_reap()
    }
    fn observe_actual_child_reaped(&self) {
        if self.main_phase.load(Ordering::Acquire) {
            self.main_reaped.store(true, Ordering::Release);
        }
        self.inner.observe_actual_child_reaped()
    }
    fn observe_never_created_proven(&self) {
        if self.main_phase.load(Ordering::Acquire) {
            self.main_never_created.store(true, Ordering::Release);
        }
        self.inner.observe_never_created_proven()
    }
    fn observe_exact_absence_proven(&self) {
        self.inner.observe_exact_absence_proven()
    }
    fn observe_cleanup_publication(&self, succeeded: bool) {
        self.inner.observe_cleanup_publication(succeeded)
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
