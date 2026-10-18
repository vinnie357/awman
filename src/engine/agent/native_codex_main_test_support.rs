//! Connected command fixture. Only the external Apple boundary and child CLI are synthetic.
use super::native_codex_auth_preparation_test::{AuthObserver, Fixture, TestResult};
use super::AgentEngine;
use crate::engine::container::gated_launch::*;
use crate::engine::container::options::{ContainerName, ImageRef};
use crate::engine::container::orchestrated_apple::{
    OrchestratedAppleTestBoundary, TestCancellationPoint, TestOwnershipObservation,
};
use crate::engine::container::ContainerRuntime;
use crate::engine::error::EngineError;
use chrono::Utc;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

const NONCE: &str = "2222222222222222222222222222222222222222222222222222222222222222";

#[derive(Default)]
struct Launch {
    created: Option<chrono::DateTime<Utc>>,
    cleaning: bool,
    removed: bool,
    reaped: bool,
    released: bool,
}

pub(crate) struct MainBoundary {
    observer: Arc<AuthObserver>,
    executable: &'static str,
    launches: Mutex<BTreeMap<String, Launch>>,
    current: Mutex<Option<String>>,
    pub records: PathBuf,
    order: Mutex<Vec<String>>,
    main_after_auth: Mutex<bool>,
}

impl MainBoundary {
    fn current(&self, update: impl FnOnce(&mut Launch)) {
        let name = self.current.lock().expect("current child").clone();
        let mut launches = self.launches.lock().expect("launch records");
        update(
            launches
                .get_mut(name.as_deref().expect("actual spawn"))
                .expect("owned launch"),
        );
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

    pub(crate) fn child_records(&self) -> TestResult<Vec<Value>> {
        let mut records = fs::read_dir(&self.records)?
            .map(|entry| {
                let entry = entry?;
                Ok(serde_json::from_slice::<Value>(&fs::read(entry.path())?)?)
            })
            .collect::<TestResult<Vec<_>>>()?;
        records.sort_by_key(|record| record["phase"].as_str().unwrap_or("").to_owned());
        Ok(records)
    }

    pub(crate) fn spawn_count(&self) -> usize {
        self.order.lock().expect("launch order").len()
    }

    pub(crate) fn first_retired(&self) -> bool {
        let order = self.order.lock().expect("launch order");
        let launches = self.launches.lock().expect("launch records");
        order
            .first()
            .and_then(|name| launches.get(name))
            .is_some_and(|launch| {
                launch.created.is_some() && launch.removed && launch.reaped && launch.released
            })
    }

    pub(crate) fn main_followed_retired_auth(&self) -> bool {
        *self.main_after_auth.lock().expect("main order")
    }

    fn unsettled(&self) -> bool {
        self.launches
            .lock()
            .expect("launch records")
            .values()
            .any(|launch| {
                launch.created.is_some() && !(launch.removed && launch.reaped && launch.released)
            })
    }
}

impl GatedProviderAdapter for MainBoundary {
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
        let launches = self.launches.lock().expect("launch records");
        if launches
            .get(name.as_str())
            .is_some_and(|launch| launch.created.is_some() && !launch.removed)
        {
            Err(NamePresence::Present)
        } else {
            Ok(self.absence(name))
        }
    }
    fn inspect_launch(&self, key: &ProviderLaunchKey, _: ProviderCallDeadline) -> ExactInspection {
        let launches = self.launches.lock().expect("launch records");
        let Some(launch) = launches.get(key.container_name.as_str()) else {
            return ExactInspection::Unavailable;
        };
        if launch.removed {
            return ExactInspection::Absent(self.absence(&key.container_name));
        }
        let Some(created_at) = launch.created else {
            return ExactInspection::Unavailable;
        };
        let runtime_id = format!("fixture-{}", key.container_name.as_str());
        let state = ProviderState::Apple(if launch.cleaning {
            AppleProviderState::Stopped
        } else {
            AppleProviderState::Running
        });
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
        inspection: &ProviderLaunchInspection,
        _: ProviderCallDeadline,
    ) -> Result<(), EngineError> {
        self.launches
            .lock()
            .expect("launch records")
            .get_mut(inspection.exact_name.as_str())
            .expect("owned stop")
            .cleaning = true;
        Ok(())
    }
    fn remove_inspected(
        &self,
        inspection: &ProviderLaunchInspection,
        _: ProviderCallDeadline,
    ) -> Result<(), EngineError> {
        self.launches
            .lock()
            .expect("launch records")
            .get_mut(inspection.exact_name.as_str())
            .expect("owned remove")
            .removed = true;
        Ok(())
    }
}

impl OrchestratedAppleTestBoundary for Arc<MainBoundary> {
    fn retention_available(&self) -> bool {
        true
    }
    fn image_inspection(&self) -> Result<Vec<u8>, EngineError> {
        serde_json::to_vec(&json!([{"variants":[{"config":{"config":{
            "User":"1000:1000", "Env":[], "Entrypoint":[],
            "Labels":{"dev.awman.startup-gate":"1"}
        }}}]}]))
        .map_err(|error| EngineError::Container(error.to_string()))
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
        let bindings = plan
            .controls
            .request
            .request
            .bindings
            .iter()
            .map(|binding| {
                json!({
                    "id": binding.id, "manifest_id": binding.manifest_id,
                    "access": binding.access, "workspace_path": binding.workspace_path,
                })
            })
            .collect::<Vec<_>>();
        let bytes = serde_json::to_vec(&json!({
            "version":2, "nonce":NONCE, "container_name":plan.key.container_name.as_str(),
            "bindings":bindings, "guest_cwd":"/workspace",
        }))
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
        _: Vec<String>,
        name: String,
        _: Vec<String>,
        _: Vec<(String, bool)>,
        registry: bool,
        _: Vec<u8>,
        _: BTreeMap<String, Vec<u8>>,
    ) {
        assert!(registry, "real registry must reach spawn");
        let has_prior = !self.order.lock().expect("launch order").is_empty();
        if has_prior {
            *self.main_after_auth.lock().expect("main order") = self.first_retired();
        }
        self.order.lock().expect("launch order").push(name.clone());
        assert!(
            self.launches
                .lock()
                .expect("launch records")
                .insert(name.clone(), Launch::default())
                .is_none(),
            "launch identity reused"
        );
        *self.current.lock().expect("current child") = Some(name);
    }
    fn observe_child_start(&self) {
        self.current(|launch| launch.created = Some(Utc::now()));
        self.observer.event("child-start");
    }
    fn observe_protected_launch_identity(&self, _: &ProviderLaunchInspection) {}
    fn observe_ready_rejected(&self) {
        self.observer.event("ready-rejected");
    }
    fn observe_protected_ready(&self) {}
    fn observe_accepted_release(&self) {}
    fn observe_provider_entry(&self, _: bool) {}
    fn observe_cancel_point(&self, _: TestCancellationPoint) {}
    fn begin_cleanup(&self) {
        self.current(|launch| launch.cleaning = true);
    }
    fn observe_reap(&self) {}
    fn observe_actual_child_reaped(&self) {
        self.current(|launch| launch.reaped = true);
        self.observer.event("actual-reap");
    }
    fn observe_never_created_proven(&self) {}
    fn observe_exact_absence_proven(&self) {}
    fn observe_cleanup_publication(&self, succeeded: bool) {
        if succeeded {
            self.observer.event("cleanup-published");
        }
    }
    fn observe_resource_loan_released(&self) {
        self.current(|launch| launch.released = true);
        self.observer.event("resources-released");
    }
    fn cleanup_reap_proven(&self) -> bool {
        true
    }
    fn observe_custody(&self, _: TestOwnershipObservation, _: bool) {}
}

pub(crate) struct MainFixture {
    pub auth: Fixture,
    pub boundary: Arc<MainBoundary>,
    probe: retention_test_support::RetentionProbe,
}
impl MainFixture {
    pub(crate) fn new() -> TestResult<Self> {
        let mut auth = Fixture::new(&[])?;
        let records = auth.path().join("actual-main-children");
        fs::create_dir(&records)?;
        let executable = auth.path().join("connected-container");
        let record_json = serde_json::to_string(&records.to_string_lossy())?;
        let script = format!(
            r##"#!/usr/bin/env python3
import json, os, pathlib, select, sys, time
args = sys.argv[1:]
volumes = [args[i + 1] for i, arg in enumerate(args[:-1]) if arg in ('-v', '--volume')]
control = pathlib.Path(next(v.rsplit(':', 1)[0] for v in volumes if v.endswith(':/.awman/startup-gate/control')))
phase = 'auth' if 'login' in args else 'main'
question = b''
deadline = time.monotonic() + 10
while not (control / 'release.json').exists():
    if time.monotonic() >= deadline: sys.exit(91)
    select.select([], [], [], 0.01)
permission = json.loads((control / 'release.json').read_text())
ack = control / ('.released.tmp.' + str(os.getpid()))
ack.write_text(json.dumps(permission))
ack.chmod(0o600)
ack.rename(control / '.released')
while True:
    ready, _, _ = select.select([sys.stdin.buffer], [], [], max(0, deadline - time.monotonic()))
    if not ready: sys.exit(92)
    chunk = os.read(0, 65536)
    if not chunk: break
    question += chunk
    if len(question) > 65536: sys.exit(93)
record = dict(phase=phase, argv=args, stdin=question.decode(), pid=os.getpid(), controls=str(control))
path = pathlib.Path({record_json}) / (phase + '.json')
with path.open('x') as output:
    json.dump(record, output)
if phase == 'auth':
    print('https://device.invalid/SYNTHETIC_DEVICE_CHALLENGE', flush=True)
else:
    print('{{"type":"thread.started","thread_id":"connected-main-fixture"}}', flush=True)
    print('{{"type":"turn.started"}}', flush=True)
    print('{{"type":"turn.completed"}}', flush=True)
"##
        );
        fs::write(&executable, script)?;
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700))?;
        let executable = Box::leak(executable.to_string_lossy().into_owned().into_boxed_str());
        let boundary = Arc::new(MainBoundary {
            observer: Arc::clone(&auth.observer),
            executable,
            launches: Mutex::new(BTreeMap::new()),
            current: Mutex::new(None),
            records,
            order: Mutex::new(Vec::new()),
            main_after_auth: Mutex::new(false),
        });
        auth.runtime = Arc::new(ContainerRuntime::apple_with_test_boundary(Arc::new(
            Arc::clone(&boundary),
        )));
        auth.engine = AgentEngine::new(Arc::clone(&auth.overlay), Arc::clone(&auth.runtime))
            .with_native_codex_auth_test_observer(Arc::clone(&auth.observer))
            .with_native_codex_main_test_opt_in();
        let registry = auth.runtime.launch_retention()?;
        let probe = retention_test_support::registry_probe(&registry);
        Ok(Self {
            auth,
            boundary,
            probe,
        })
    }
}
impl Drop for MainFixture {
    fn drop(&mut self) {
        let retained = self.probe.snapshot();
        if self.boundary.unsettled() || retained.retained != 0 || retained.unreaped != 0 {
            // The original fixture alone keeps its root; its Drop also owns panic handling.
            self.auth
                .observer
                .teardown_uncertain
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
}
