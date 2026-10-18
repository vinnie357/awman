//! Private Packet2 proposal: current bytes through the real scope/retirement route.
use super::native_codex_auth_preparation_test::{AuthObserver, TestResult};
use super::native_codex_main_a2a_test_support::{is_main, A2Fixture};
use super::native_codex_main_a2m_test_support::Materials;
use super::AgentEngine;
use crate::engine::container::gated_launch::*;
use crate::engine::container::orchestrated_apple::*;
use crate::engine::container::ContainerRuntime;
use crate::engine::error::EngineError;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
const SDK_BYTES: &[u8] = b"SDK marker written after actual auth resource release";
const WORKSPACE_BYTES: &[u8] = b"known workspace bytes before actual command";
struct Snapshot {
    main: bool,
    request: Value,
    manifests: BTreeMap<String, Vec<u8>>,
    sdk: PathBuf,
}
pub(crate) struct Freshness {
    inner: Arc<Materials>,
    observer: Arc<AuthObserver>,
    snapshots: Mutex<Vec<Snapshot>>,
}
pub(crate) fn wrap(f: &mut A2Fixture, scope: Arc<Materials>) -> TestResult<Arc<Freshness>> {
    fs::write(
        f.fixture
            .auth
            .session
            .git_root()
            .join("a2m-fresh-workspace"),
        WORKSPACE_BYTES,
    )?;
    let outer = Arc::new(Freshness {
        inner: scope,
        observer: Arc::clone(&f.fixture.auth.observer),
        snapshots: Mutex::new(Vec::new()),
    });
    f.fixture.auth.runtime = Arc::new(ContainerRuntime::apple_with_test_boundary(outer.clone()));
    let engine = AgentEngine::new(
        Arc::clone(&f.fixture.auth.overlay),
        Arc::clone(&f.fixture.auth.runtime),
    )
    .with_native_codex_auth_test_observer(Arc::clone(&f.fixture.auth.observer))
    .with_native_codex_main_test_opt_in();
    f.fixture.auth.engine = engine;
    Ok(outer)
}
impl OrchestratedAppleTestBoundary for Freshness {
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
        self.snapshots
            .lock()
            .expect("freshness snapshots")
            .push(Snapshot {
                main: is_main(&argv),
                request: serde_json::from_slice(&request).expect("actual protected request"),
                manifests: manifests.clone(),
                sdk: self.observer.state_path(),
            });
        self.inner
            .observe_spawn(argv, name, labels, mounts, registry, request, manifests);
    }
    fn observe_resource_loan_released(&self) {
        self.inner.observe_resource_loan_released();
        let snapshots = self.snapshots.lock().expect("freshness snapshots");
        let actual = snapshots
            .last()
            .expect("actual spawn before resource release");
        if !actual.main {
            self.observer.assert_locked();
            let path = actual.sdk.join("a2m-post-auth-freshness");
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .expect("new post-auth marker");
            file.write_all(SDK_BYTES)
                .expect("bounded post-auth SDK bytes");
        }
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
        self.inner.observe_actual_child_reaped()
    }
    fn observe_never_created_proven(&self) {
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
fn expected_entry(raw: &[u8], name: &str, bytes: &[u8]) -> TestResult {
    let manifest: Value = serde_json::from_slice(raw)?;
    let entries = manifest["entries"].as_array().ok_or("manifest entries")?;
    let matched = entries
        .iter()
        .filter(|entry| entry["path"] == name)
        .collect::<Vec<_>>();
    assert_eq!(matched.len(), 1, "exact expected manifest entry");
    assert_eq!(matched[0]["path"], name);
    assert_eq!(matched[0]["kind"], "file");
    assert_eq!(matched[0]["size"], bytes.len());
    assert_eq!(
        matched[0]["sha256"],
        lower_hex(&Sha256::digest(bytes).into())
    );
    Ok(())
}
pub(crate) fn assert_freshness(f: &A2Fixture, observed: &Freshness) -> TestResult {
    let snapshots = observed.snapshots.lock().expect("freshness snapshots");
    assert_eq!(snapshots.len(), 2);
    let (auth, main) = (&snapshots[0], &snapshots[1]);
    assert!(!auth.main && main.main);
    assert_eq!(auth.sdk, main.sdk);
    assert_eq!(auth.sdk, f.fixture.auth.observer.state_path());
    let bindings = main.request["bindings"].as_array().ok_or("main bindings")?;
    for (target, name, bytes) in [
        (
            "/data/awman-native-sdk/codex",
            "a2m-post-auth-freshness",
            SDK_BYTES,
        ),
        ("/workspace", "a2m-fresh-workspace", WORKSPACE_BYTES),
    ] {
        let binding = bindings
            .iter()
            .find(|binding| binding["workspace_path"] == target)
            .ok_or("actual main binding")?;
        let manifest = main
            .manifests
            .get(
                binding["manifest_file"]
                    .as_str()
                    .ok_or("manifest reference")?,
            )
            .ok_or("actual main manifest")?;
        expected_entry(manifest, name, bytes)?;
    }
    assert_eq!(
        fs::read(auth.sdk.join("a2m-post-auth-freshness"))?,
        SDK_BYTES
    );
    assert_eq!(
        fs::read(
            f.fixture
                .auth
                .session
                .git_root()
                .join("a2m-fresh-workspace")
        )?,
        WORKSPACE_BYTES
    );
    Ok(())
}
