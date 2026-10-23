//! A2m scope packet observes exact mounts and actual protected launch identity.
use super::native_codex_auth_preparation_test::{AuthObserver, TestResult};
use super::native_codex_main_a2a_test_support::{A2Fixture, ObservedBoundary};
use super::AgentEngine;
use crate::data::startup_gate::revalidate_mount_source;
use crate::engine::container::gated_launch::*;
use crate::engine::container::orchestrated_apple::*;
use crate::engine::container::ContainerRuntime;
use crate::engine::error::EngineError;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
const SDK: &str = "/data/awman-native-sdk/codex";
const BIN: &str = "/.awman/startup-gate/bin";
const CONTROL: &str = "/.awman/startup-gate/control";
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Mount(PathBuf, String, bool);
struct Material {
    name: String,
    mounts: Vec<Mount>,
    launch_labels: Vec<String>,
    request: Value,
    manifests: BTreeMap<String, Vec<u8>>,
    bootstrap: PathBuf,
    sdk: PathBuf,
}
pub(crate) struct Materials {
    inner: Arc<ObservedBoundary>,
    observer: Arc<AuthObserver>,
    records: Mutex<Vec<Material>>,
    plans: Mutex<BTreeMap<String, (PathBuf, [u8; 32])>>,
    inspections: Mutex<BTreeMap<String, [u8; 32]>>,
}
pub(crate) fn instrument(f: &mut A2Fixture) -> TestResult<Arc<Materials>> {
    let observed = Arc::new(Materials {
        inner: Arc::clone(&f.boundary),
        observer: Arc::clone(&f.fixture.auth.observer),
        records: Mutex::new(Vec::new()),
        plans: Mutex::new(BTreeMap::new()),
        inspections: Mutex::new(BTreeMap::new()),
    });
    f.fixture.auth.runtime = Arc::new(ContainerRuntime::apple_with_test_boundary(observed.clone()));
    let engine = AgentEngine::new(
        Arc::clone(&f.fixture.auth.overlay),
        Arc::clone(&f.fixture.auth.runtime),
    )
    .with_native_codex_auth_test_observer(Arc::clone(&f.fixture.auth.observer))
    .with_native_codex_main_test_opt_in();
    f.fixture.auth.engine = engine;
    Ok(observed)
}
fn parse_mounts(argv: &[String], name: &str) -> (Vec<Mount>, Vec<String>) {
    let wrappers = argv
        .windows(4)
        .enumerate()
        .filter(|(_, p)| {
            p[0] == "/usr/bin/python3"
                && p[1] == "-I"
                && p[2] == "-S"
                && p[3] == format!("{BIN}/bootstrap.py")
        })
        .map(|(i, _)| i)
        .collect::<Vec<_>>();
    assert_eq!(wrappers.len(), 1);
    let image = wrappers[0].checked_sub(1).expect("image before wrapper");
    assert!(!argv[image].is_empty() && !argv[image].starts_with('-'));
    let prefix = &argv[..image];
    assert_eq!(argv[wrappers[0] + 4], CONTROL);
    let mut names = Vec::new();
    let mut launch_labels = Vec::new();
    let mut args = prefix.iter();
    let mut mounts = Vec::new();
    while let Some(arg) = args.next() {
        if arg == "-v" {
            let raw = args.next().expect("mount value");
            let parts = raw.split(':').collect::<Vec<_>>();
            assert!(parts.len() == 2 || (parts.len() == 3 && parts[2] == "ro"));
            assert!(parts[0].starts_with('/') && parts[1].starts_with('/'));
            mounts.push(Mount(
                PathBuf::from(parts[0]),
                parts[1].into(),
                parts.len() == 3,
            ));
        } else if matches!(
            arg.as_str(),
            "--name" | "--user" | "--label" | "-w" | "-e" | "--cpus" | "--memory"
        ) {
            let value = args.next().expect("container option value");
            if arg == "--name" {
                names.push(value.as_str());
            }
            if arg == "--label" {
                assert_ne!(
                    value, "dev.awman.orchestrator-launch",
                    "reserved launch label requires a value"
                );
                if let Some(digest) = value.strip_prefix("dev.awman.orchestrator-launch=") {
                    launch_labels.push(digest.to_owned());
                }
            }
        } else {
            assert!(
                matches!(arg.as_str(), "run" | "--rm" | "-i" | "-it"),
                "unsupported container option: {arg}"
            );
        }
    }
    assert_eq!(names, [name]);
    (mounts, launch_labels)
}
fn private_metadata(path: &Path, directory: bool) -> TestResult {
    let m = fs::symlink_metadata(path)?;
    assert_eq!(m.uid(), nix::unistd::geteuid().as_raw());
    assert_eq!(m.mode() & 0o7777, if directory { 0o700 } else { 0o600 });
    assert_eq!(m.is_dir(), directory);
    assert!(directory || (m.is_file() && m.nlink() == 1));
    assert!(!m.file_type().is_symlink());
    Ok(())
}
fn bootstrap(
    mounts: &[Mount],
    request: &[u8],
    manifests: &BTreeMap<String, Vec<u8>>,
) -> TestResult<PathBuf> {
    let bins = mounts.iter().filter(|m| m.1 == BIN).collect::<Vec<_>>();
    assert_eq!(bins.len(), 1);
    assert!(bins[0].2);
    let root = &bins[0].0;
    private_metadata(root, true)?;
    assert!(root
        .file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.starts_with("awman-startup-gate-")));
    assert_eq!(
        fs::canonicalize(root)?.parent(),
        Some(fs::canonicalize(std::env::temp_dir())?.as_path())
    );
    let mut expected = BTreeMap::from([
        (
            "bootstrap.py".to_string(),
            include_bytes!("../container/startup_gate_bootstrap.py").to_vec(),
        ),
        ("request.json".into(), request.to_vec()),
        ("original-python-env.json".into(), b"{}".to_vec()),
    ]);
    for (name, bytes) in manifests {
        assert!(matches!(
            Path::new(name).components().collect::<Vec<_>>().as_slice(),
            [std::path::Component::Normal(_)]
        ));
        assert!(
            expected.insert(name.clone(), bytes.clone()).is_none(),
            "reserved manifest name"
        );
    }
    let actual = fs::read_dir(root)?
        .map(|entry| {
            Ok(entry?
                .file_name()
                .into_string()
                .map_err(|_| "non-UTF8 staged name")?)
        })
        .collect::<TestResult<BTreeSet<_>>>()?;
    assert_eq!(actual, expected.keys().cloned().collect());
    for (name, bytes) in expected {
        let path = root.join(name);
        private_metadata(&path, false)?;
        assert_eq!(fs::read(path)?, bytes);
    }
    Ok(root.clone())
}
impl OrchestratedAppleTestBoundary for Materials {
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
        let (parsed, launch_labels) = parse_mounts(&argv, &name);
        assert_eq!(
            launch_labels, labels,
            "actual CLI launch labels differ from metadata"
        );
        let staged =
            bootstrap(&parsed, &request, &manifests).expect("actual private bootstrap scope");
        self.records.lock().expect("materials").push(Material {
            name: name.clone(),
            mounts: parsed,
            launch_labels,
            request: serde_json::from_slice(&request).expect("protected request"),
            manifests: manifests.clone(),
            bootstrap: staged,
            sdk: self.observer.state_path(),
        });
        self.inner
            .observe_spawn(argv, name, labels, mounts, registry, request, manifests);
    }
    fn prepare_simulated_guest(
        &self,
        plan: &DurableLaunchPlan,
    ) -> Result<(), LaunchRetentionReason> {
        let source = revalidate_mount_source(&plan.controls)
            .map_err(|_| LaunchRetentionReason::PostSpawnControlChanged)?;
        let evidence = (source, plan.key.token_digest);
        let prior = self
            .plans
            .lock()
            .expect("plans")
            .insert(plan.key.container_name.as_str().into(), evidence.clone());
        assert!(prior.is_none_or(|old| old == evidence));
        self.inner.prepare_simulated_guest(plan)
    }
    fn observe_protected_launch_identity(&self, inspection: &ProviderLaunchInspection) {
        let prior = self.inspections.lock().expect("inspections").insert(
            inspection.exact_name.as_str().into(),
            inspection.token_digest,
        );
        assert!(prior.is_none_or(|old| old == inspection.token_digest));
        self.inner.observe_protected_launch_identity(inspection);
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
    fn pause_before_ready(&self) -> bool {
        self.inner.pause_before_ready()
    }
    fn pause_after_ready(&self) -> bool {
        self.inner.pause_after_ready()
    }
    fn observe_child_start(&self) {
        self.inner.observe_child_start()
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
    fn cleanup_reap_proven(&self) -> bool {
        self.inner.cleanup_reap_proven()
    }
    fn observe_custody(&self, ownership: TestOwnershipObservation, legacy: bool) {
        self.inner.observe_custody(ownership, legacy)
    }
}
fn hash(bytes: &[u8]) -> String {
    lower_hex(&Sha256::digest(bytes).into())
}
fn exclude_owner_records(manifest: &Value) {
    let entries = manifest["entries"].as_array().expect("manifest entries");
    for entry in entries {
        assert!(![
            "owner.json",
            "active-run.json",
            "completed-run.json",
            "writer.lock",
            "guest-control"
        ]
        .iter()
        .any(|reserved| entry["path"]
            .as_str()
            .expect("entry path")
            .split('/')
            .any(|part| part == *reserved)));
    }
}
pub(crate) fn assert_materials(f: &A2Fixture, observed: &Materials) -> TestResult {
    let records = observed.records.lock().expect("materials");
    assert_eq!(records.len(), 2);
    let (auth, main) = (&records[0], &records[1]);
    let plans = observed.plans.lock().expect("plans");
    let inspections = observed.inspections.lock().expect("inspections");
    assert_eq!(plans.len(), 2);
    assert_ne!(auth.name, main.name);
    let a = plans.get(&auth.name).ok_or("auth protected plan")?;
    let m = plans.get(&main.name).ok_or("main protected plan")?;
    assert_ne!(a.0, m.0);
    assert_ne!(a.1, m.1);
    assert_eq!(auth.launch_labels, [lower_hex(&a.1)]);
    assert_eq!(main.launch_labels, [lower_hex(&m.1)]);
    assert_eq!(inspections.get(&auth.name), Some(&a.1));
    assert_eq!(inspections.get(&main.name), Some(&m.1));
    assert_eq!(auth.sdk, main.sdk);
    let project = fs::canonicalize(f.fixture.auth.session.git_root())?;
    let mut expected = [
        Mount(main.sdk.clone(), SDK.into(), false),
        Mount(project, "/workspace".into(), true),
        Mount(main.bootstrap.clone(), BIN.into(), true),
        Mount(m.0.clone(), CONTROL.into(), false),
    ];
    expected.sort();
    let mut actual = main.mounts.iter().collect::<Vec<_>>();
    actual.sort();
    assert_eq!(actual, expected.iter().collect::<Vec<_>>());
    let bindings = main.request["bindings"].as_array().ok_or("bindings")?;
    assert_eq!(bindings.len(), 2);
    let sdk = bindings
        .iter()
        .find(|b| b["workspace_path"] == SDK)
        .ok_or("SDK binding")?;
    let workspace = bindings
        .iter()
        .find(|b| b["workspace_path"] == "/workspace")
        .ok_or("project binding")?;
    assert_eq!(sdk["access"], "read-write");
    assert_eq!(workspace["access"], "read-only");
    assert_ne!(sdk["id"], workspace["id"]);
    assert_ne!(sdk["manifest_file"], workspace["manifest_file"]);
    assert_eq!(main.manifests.len(), 2);
    for binding in [sdk, workspace] {
        let raw = main
            .manifests
            .get(
                binding["manifest_file"]
                    .as_str()
                    .ok_or("manifest reference")?,
            )
            .ok_or("manifest bytes")?;
        assert_eq!(binding["manifest_id"], hash(raw));
        exclude_owner_records(&serde_json::from_slice(raw)?);
    }
    Ok(())
}
