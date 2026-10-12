use super::orchestrated_apple_command_acceptance_test::{
    AppleCommandEffects, AppleCommandExternalPlan, AppleCommandObserver, CancellationPoint,
    CleanupFault, CleanupIdentitySnapshot, CleanupStep, MountObservation, OwnershipObservation,
    PlannedLaunchIdentitySnapshot, QualificationFault, RetryFaultPoint, SecondInspectionRelease,
};
use crate::engine::container::gated_launch::{
    canonical_inspection_revision, AbsenceObservation, AppleProviderState,
    CanonicalInspectionRevisionInput, CreateCliExitClass, CreateCliExitEvidence, DurableLaunchPlan,
    ExactInspection, GatedProviderAdapter, ImmutableImageId, InspectionObservationKind,
    LaunchIdentityError, LaunchRetentionReason, NamePresence, ProviderCallDeadline, ProviderKind,
    ProviderLaunchInspection, ProviderLaunchKey, ProviderState, SanitizedProviderStateObservation,
};
use crate::engine::container::options::{ContainerName, ImageRef};
use crate::engine::container::orchestrated_apple::{
    OrchestratedAppleTestBoundary, TestCancellationPoint, TestOwnershipObservation,
};
use crate::engine::container::ContainerRuntime;
use crate::engine::error::EngineError;
use chrono::Utc;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

const NONCE: &str = "2222222222222222222222222222222222222222222222222222222222222222";

impl ContainerRuntime {
    pub(crate) fn apple_with_orchestrated_command_test_boundary(
        plan: AppleCommandExternalPlan,
        observer: AppleCommandObserver,
        effects: AppleCommandEffects,
    ) -> Self {
        let boundary = Arc::new(TestBoundary::new(plan, observer, effects));
        Self::apple_with_test_boundary(boundary)
    }
}

struct TestBoundary {
    plan: AppleCommandExternalPlan,
    observer: AppleCommandObserver,
    adapter: Arc<TestProviderAdapter>,
    _cli_dir: Option<tempfile::TempDir>,
    executable: &'static str,
}

impl TestBoundary {
    fn new(
        plan: AppleCommandExternalPlan,
        observer: AppleCommandObserver,
        effects: AppleCommandEffects,
    ) -> Self {
        let script = format!(
            "#!/bin/sh\ncontrol=''\nprev=''\nfor arg in \"$@\"; do\n  if [ \"$prev\" = '-v' ]; then\n    case \"$arg\" in *:/.awman/startup-gate/control) control=${{arg%:/.awman/startup-gate/control}} ;; esac\n  fi\n  prev=$arg\ndone\nif [ -z \"$control\" ]; then exit 0; fi\nif [ ! -f \"$control/../launch-plan.json\" ]; then exit 0; fi\nwhile [ ! -f \"$control/release.json\" ]; do sleep 0.01; done\numask 077\nreleased_tmp=\"$control/.released.tmp.$$\"\nprintf '%s' '{{\"nonce\":\"{NONCE}\",\"version\":1}}' > \"$released_tmp\" || exit 1\nmv \"$released_tmp\" \"$control/.released\" || exit 1\nprintf 'entered\\n' >> \"$control/provider-entry.marker\"\nsleep 0.05\nexit 0\n"
        );
        let (cli_dir, executable) = match tempfile::tempdir() {
            Ok(cli_dir) => {
                let executable_path = cli_dir.path().join("container");
                let installed = std::fs::File::create(&executable_path)
                    .and_then(|mut file| {
                        file.write_all(script.as_bytes())?;
                        file.sync_all()
                    })
                    .and_then(|()| {
                        std::fs::set_permissions(
                            &executable_path,
                            std::fs::Permissions::from_mode(0o700),
                        )
                    })
                    .is_ok();
                if installed {
                    let executable: &'static str = Box::leak(
                        executable_path
                            .to_string_lossy()
                            .into_owned()
                            .into_boxed_str(),
                    );
                    (Some(cli_dir), executable)
                } else {
                    (Some(cli_dir), "/usr/bin/false")
                }
            }
            Err(_) => (None, "/usr/bin/false"),
        };
        let adapter = Arc::new(TestProviderAdapter::new(
            plan,
            observer.clone(),
            effects.clone(),
        ));
        Self {
            plan,
            observer,
            adapter,
            _cli_dir: cli_dir,
            executable,
        }
    }
}

impl OrchestratedAppleTestBoundary for TestBoundary {
    fn retention_available(&self) -> bool {
        !matches!(self.plan, AppleCommandExternalPlan::RegistryUnavailable)
    }

    fn image_inspection(&self) -> Result<Vec<u8>, EngineError> {
        let (trusted, user, env, entrypoint) = match self.plan {
            AppleCommandExternalPlan::Image(QualificationFault::UntrustedImage) => {
                (false, "1000:1000", Vec::new(), Vec::new())
            }
            AppleCommandExternalPlan::Image(QualificationFault::ImageEntrypoint) => {
                (true, "1000:1000", Vec::new(), vec!["/bad"])
            }
            AppleCommandExternalPlan::Image(QualificationFault::ImageLoaderEnvironment) => {
                (true, "1000:1000", vec!["PYTHONPATH=/bad"], Vec::new())
            }
            AppleCommandExternalPlan::Image(QualificationFault::MissingRuntimeUser) => {
                (true, "", Vec::new(), Vec::new())
            }
            AppleCommandExternalPlan::Image(QualificationFault::InvalidRuntimeUser) => {
                (true, "agent", Vec::new(), Vec::new())
            }
            _ => (true, "1000:1000", Vec::new(), Vec::new()),
        };
        self.observer
            .observe_image(trusted, (!user.is_empty()).then(|| user.to_string()));
        serde_json::to_vec(&serde_json::json!([{
            "variants": [{"config": {"config": {
                "User": user,
                "Env": env,
                "Entrypoint": entrypoint,
                "Labels": if trusted { serde_json::json!({"dev.awman.startup-gate": "1"}) } else { serde_json::json!({}) }
            }}}]
        }]))
        .map_err(|error| EngineError::Container(error.to_string()))
    }

    fn provider(&self) -> Arc<dyn GatedProviderAdapter> {
        self.adapter.clone()
    }

    fn executable(&self) -> &'static str {
        self.executable
    }

    fn prepare_simulated_guest(
        &self,
        plan: &DurableLaunchPlan,
    ) -> Result<(), LaunchRetentionReason> {
        self.observer.observe_planned_launch_identity(
            TestProviderAdapter::planned_identity_snapshot(&plan.key),
        );
        if self.pause_before_ready() {
            return Ok(());
        }
        let bindings = plan
            .controls
            .request
            .request
            .bindings
            .iter()
            .map(|binding| {
                serde_json::json!({
                    "id": binding.id.clone(),
                    "manifest_id": binding.manifest_id.clone(),
                    "access": binding.access,
                    "workspace_path": binding.workspace_path.clone(),
                })
            })
            .collect::<Vec<_>>();
        let bytes = serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "nonce": NONCE,
            "container_name": plan.key.container_name.as_str(),
            "bindings": bindings,
        }))
        .map_err(|_| LaunchRetentionReason::PostSpawnControlChanged)?;
        plan.controls
            .publish_guest_record("ready.json", ".ready", &bytes)
            .map_err(|_| LaunchRetentionReason::PostSpawnControlChanged)?;
        let controls = Arc::clone(&plan.controls);
        let observer = self.observer.clone();
        let _ = std::thread::Builder::new()
            .name("awman-provider-entry-observer".into())
            .spawn(move || {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
                while std::time::Instant::now() < deadline {
                    match controls.read_guest_record("provider-entry.marker", 64) {
                        Ok(Some(bytes)) if bytes == b"entered\n" => {
                            observer.observe_provider_entry(false);
                            return;
                        }
                        Ok(Some(_)) | Err(_) => return,
                        Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
                    }
                }
            });
        Ok(())
    }

    fn pause_before_ready(&self) -> bool {
        matches!(
            self.plan,
            AppleCommandExternalPlan::Cancel(CancellationPoint::BeforeReady)
        )
    }

    fn pause_after_ready(&self) -> bool {
        matches!(
            self.plan,
            AppleCommandExternalPlan::Cancel(CancellationPoint::AfterReadyBeforeRelease)
        )
    }

    fn observe_spawn(
        &self,
        argv: Vec<String>,
        container_name: String,
        labels: Vec<String>,
        writable_mounts: Vec<(String, bool)>,
        registry_arc_reached_spawn: bool,
        staged_request: Vec<u8>,
        staged_manifests: std::collections::BTreeMap<String, Vec<u8>>,
    ) {
        self.observer
            .observe_staged_snapshot(staged_request, staged_manifests, false);
        self.observer.observe_spawn(
            argv,
            container_name,
            labels,
            writable_mounts
                .into_iter()
                .map(|(container_path, source_revalidated)| MountObservation {
                    container_path,
                    writable: true,
                    source_revalidated,
                })
                .collect(),
            registry_arc_reached_spawn,
        );
    }

    fn observe_child_start(&self) {
        self.adapter.created_at.get_or_init(Utc::now);
        self.adapter.created.store(true, Ordering::Release);
        self.observer.observe_child_start();
    }

    fn observe_protected_launch_identity(&self, inspection: &ProviderLaunchInspection) {
        self.observer
            .observe_protected_launch_identity(TestProviderAdapter::identity_snapshot(inspection));
    }

    fn observe_protected_ready(&self) {
        self.observer.observe_protected_ready();
    }

    fn observe_accepted_release(&self) {
        self.observer.observe_accepted_release();
    }

    fn observe_provider_entry(&self, before_release: bool) {
        self.observer.observe_provider_entry(before_release);
    }

    fn observe_cancel_point(&self, point: TestCancellationPoint) {
        self.observer.observe_cancel_point(match point {
            TestCancellationPoint::BeforeReady => CancellationPoint::BeforeReady,
            TestCancellationPoint::AfterReadyBeforeRelease => {
                CancellationPoint::AfterReadyBeforeRelease
            }
        });
    }

    fn begin_cleanup(&self) {
        self.adapter.cleanup.store(true, Ordering::Release);
        self.adapter.cleanup_inspections.store(0, Ordering::Release);
    }

    fn observe_reap(&self) {
        self.observer.observe_provider_call(Some(CleanupStep::Reap));
    }

    fn cleanup_reap_proven(&self) -> bool {
        !matches!(
            self.plan,
            AppleCommandExternalPlan::CleanupUncertain(CleanupFault::ReapUncertain)
        )
    }

    fn observe_custody(&self, ownership: TestOwnershipObservation, legacy: bool) {
        self.observer.observe_custody(
            OwnershipObservation {
                exact_plan: ownership.exact_plan,
                child: ownership.child,
                container: ownership.container,
                controls: ownership.controls,
                staged_files: ownership.staged_files,
                registry_ticket: ownership.registry_ticket,
                quiescence_proven: ownership.quiescence_proven,
            },
            false,
            false,
            legacy,
        );
    }
}

struct TestProviderAdapter {
    plan: AppleCommandExternalPlan,
    observer: AppleCommandObserver,
    cleanup: AtomicBool,
    cleanup_inspections: AtomicUsize,
    created: AtomicBool,
    stopped: AtomicBool,
    removed: AtomicBool,
    created_at: OnceLock<chrono::DateTime<Utc>>,
    launch_inspections: AtomicUsize,
    effects: AppleCommandEffects,
}

impl TestProviderAdapter {
    fn new(
        plan: AppleCommandExternalPlan,
        observer: AppleCommandObserver,
        effects: AppleCommandEffects,
    ) -> Self {
        Self {
            plan,
            observer,
            cleanup: AtomicBool::new(false),
            cleanup_inspections: AtomicUsize::new(0),
            created: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            removed: AtomicBool::new(false),
            created_at: OnceLock::new(),
            launch_inspections: AtomicUsize::new(0),
            effects,
        }
    }

    fn identity_snapshot(inspection: &ProviderLaunchInspection) -> CleanupIdentitySnapshot {
        CleanupIdentitySnapshot {
            provider: match inspection.provider {
                ProviderKind::Docker => "docker",
                ProviderKind::AppleContainers => "apple-containers",
            }
            .into(),
            runtime_id: inspection.runtime_id.clone(),
            exact_name: inspection.exact_name.as_str().to_string(),
            token_digest: inspection
                .token_digest
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
            immutable_image_id: inspection.immutable_image_id.as_str().to_string(),
            created_at: inspection.created_at.to_owned(),
        }
    }

    fn planned_identity_snapshot(key: &ProviderLaunchKey) -> PlannedLaunchIdentitySnapshot {
        PlannedLaunchIdentitySnapshot {
            provider: match key.provider {
                ProviderKind::Docker => "docker",
                ProviderKind::AppleContainers => "apple-containers",
            }
            .into(),
            exact_name: key.container_name.as_str().to_string(),
            token_digest: key
                .token_digest
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
            immutable_image_id: key.immutable_image_id.as_str().to_string(),
            created_not_before: key.created_not_before.to_owned(),
        }
    }

    fn matching_with_identity(
        &self,
        key: &ProviderLaunchKey,
        stopped: bool,
        runtime_id: String,
        created_at: chrono::DateTime<Utc>,
    ) -> ProviderLaunchInspection {
        let state = ProviderState::Apple(if stopped {
            AppleProviderState::Stopped
        } else {
            AppleProviderState::Running
        });
        let revision = canonical_inspection_revision(CanonicalInspectionRevisionInput {
            kind: InspectionObservationKind::Matching,
            provider: ProviderKind::AppleContainers,
            exact_name: &key.container_name,
            runtime_id: Some(&runtime_id),
            token_digest: Some(key.token_digest),
            immutable_image_id: Some(&key.immutable_image_id),
            created_at: Some(created_at),
            state: SanitizedProviderStateObservation::Known(state.clone()),
        });
        ProviderLaunchInspection {
            provider: ProviderKind::AppleContainers,
            runtime_id,
            exact_name: key.container_name.clone(),
            token_digest: key.token_digest,
            immutable_image_id: key.immutable_image_id.clone(),
            created_at,
            state,
            revision,
        }
    }

    fn matching_with_captured_identity(
        &self,
        key: &ProviderLaunchKey,
        stopped: bool,
        runtime_id: String,
        created_at_offset: chrono::Duration,
    ) -> ExactInspection {
        let Some(created_at) = self.created_at.get() else {
            return ExactInspection::Unavailable;
        };
        ExactInspection::Matching(self.matching_with_identity(
            key,
            stopped,
            runtime_id,
            created_at.to_owned() + created_at_offset,
        ))
    }

    fn matching(&self, key: &ProviderLaunchKey, stopped: bool) -> ExactInspection {
        self.matching_with_captured_identity(
            key,
            stopped,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            chrono::Duration::zero(),
        )
    }

    fn absent(&self, name: &ContainerName) -> AbsenceObservation {
        let checked_at = Utc::now();
        AbsenceObservation {
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
        }
    }
}

impl GatedProviderAdapter for TestProviderAdapter {
    fn provider(&self) -> ProviderKind {
        ProviderKind::AppleContainers
    }

    fn resolve_image_identity(
        &self,
        _image: &ImageRef,
        _deadline: ProviderCallDeadline,
    ) -> Result<ImmutableImageId, LaunchIdentityError> {
        self.observer.observe_provider_call(None);
        ImmutableImageId::new(format!("sha256:{}", "0".repeat(64)))
    }

    fn inspect_name_absence(
        &self,
        name: &ContainerName,
        _deadline: ProviderCallDeadline,
    ) -> Result<AbsenceObservation, NamePresence> {
        let removed = self.removed.load(Ordering::Acquire);
        self.observer.observe_provider_call(if removed {
            Some(CleanupStep::ProveAbsent)
        } else {
            None
        });
        if removed
            && matches!(
                self.plan,
                AppleCommandExternalPlan::CleanupUncertain(CleanupFault::FinalAbsenceUnproven)
            )
        {
            return Err(NamePresence::Unavailable);
        }
        if removed
            && matches!(
                self.plan,
                AppleCommandExternalPlan::RepeatRetry(RetryFaultPoint::FinalAbsence)
                    | AppleCommandExternalPlan::SupervisorRetry(RetryFaultPoint::FinalAbsence)
            )
            && self.effects.retry_fault_is_active()
        {
            return Err(NamePresence::Unavailable);
        }
        if self.created.load(Ordering::Acquire) && !removed {
            return Err(NamePresence::Present);
        }
        Ok(self.absent(name))
    }

    fn inspect_launch(
        &self,
        key: &ProviderLaunchKey,
        deadline: ProviderCallDeadline,
    ) -> ExactInspection {
        if !self.cleanup.load(Ordering::Acquire) {
            self.observer.observe_provider_call(None);
            if matches!(self.plan, AppleCommandExternalPlan::SpawnAmbiguous) {
                return ExactInspection::Unavailable;
            }
            if !self.created.load(Ordering::Acquire) {
                return ExactInspection::Absent(self.absent(&key.container_name));
            }
            let index = self.launch_inspections.fetch_add(1, Ordering::AcqRel);
            if matches!(self.plan, AppleCommandExternalPlan::ReceiptSecondInspection) && index == 1
            {
                return match self.effects.wait_at_second_inspection(deadline.instant()) {
                    SecondInspectionRelease::Matching => self.matching(key, false),
                    SecondInspectionRelease::Unavailable => ExactInspection::Unavailable,
                };
            }
            return self.matching(key, false);
        }
        self.observer
            .observe_provider_call(Some(CleanupStep::Inspect));
        if self.removed.load(Ordering::Acquire) {
            return ExactInspection::Absent(self.absent(&key.container_name));
        }
        let index = self.cleanup_inspections.fetch_add(1, Ordering::AcqRel);
        let result = match (self.plan, index) {
            (AppleCommandExternalPlan::CleanupUncertain(CleanupFault::TupleChanged), 0) => {
                ExactInspection::ForeignOrAmbiguous
            }
            (
                AppleCommandExternalPlan::CleanupUncertain(CleanupFault::InspectionUnavailable),
                0,
            )
            | (
                AppleCommandExternalPlan::DropWithoutWait(
                    super::orchestrated_apple_command_acceptance_test::RetentionRetryFault::InspectionUnavailable,
                ),
                0,
            )
            | (
                AppleCommandExternalPlan::CleanupUncertain(
                    CleanupFault::PreRemoveInspectionUnavailable,
                ),
                1,
            ) => ExactInspection::Unavailable,
            (
                AppleCommandExternalPlan::CleanupUncertain(
                    CleanupFault::PreRemoveRuntimeIdChanged,
                ),
                1,
            ) => self.matching_with_captured_identity(
                key,
                true,
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
                chrono::Duration::zero(),
            ),
            (
                AppleCommandExternalPlan::CleanupUncertain(
                    CleanupFault::PreRemoveCreatedAtChanged,
                ),
                1,
            ) => self.matching_with_captured_identity(
                key,
                true,
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                chrono::Duration::seconds(1),
            ),
            (
                AppleCommandExternalPlan::RepeatRetry(RetryFaultPoint::CleanupInspection),
                0,
            ) if self.effects.retry_fault_is_active() => ExactInspection::Unavailable,
            _ => self.matching(key, self.stopped.load(Ordering::Acquire)),
        };
        if let ExactInspection::Matching(inspection) = &result {
            self.observer.observe_cleanup_identity(
                CleanupStep::Inspect,
                Self::identity_snapshot(inspection),
            );
        }
        result
    }

    fn classify_create_exit(&self, _exit: &CreateCliExitEvidence) -> CreateCliExitClass {
        CreateCliExitClass::Other
    }

    fn stop_inspected(
        &self,
        inspection: &ProviderLaunchInspection,
        _deadline: ProviderCallDeadline,
    ) -> Result<(), EngineError> {
        self.observer
            .observe_cleanup_identity(CleanupStep::Stop, Self::identity_snapshot(inspection));
        self.observer.observe_provider_call(Some(CleanupStep::Stop));
        if matches!(
            self.plan,
            AppleCommandExternalPlan::CleanupUncertain(CleanupFault::StopFailed)
        ) {
            Err(EngineError::Container("injected stop uncertainty".into()))
        } else {
            self.stopped.store(true, Ordering::Release);
            Ok(())
        }
    }

    fn remove_inspected(
        &self,
        inspection: &ProviderLaunchInspection,
        _deadline: ProviderCallDeadline,
    ) -> Result<(), EngineError> {
        self.observer
            .observe_cleanup_identity(CleanupStep::Remove, Self::identity_snapshot(inspection));
        self.observer
            .observe_provider_call(Some(CleanupStep::Remove));
        if matches!(
            self.plan,
            AppleCommandExternalPlan::CleanupUncertain(CleanupFault::RemoveFailed)
        ) {
            return Err(EngineError::Container("injected remove uncertainty".into()));
        }
        self.removed.store(true, Ordering::Release);
        if matches!(
            self.plan,
            AppleCommandExternalPlan::RepeatRetry(RetryFaultPoint::RemoveEffectThenError)
        ) && self.effects.retry_fault_is_active()
        {
            return Err(EngineError::Container(
                "injected remove response uncertainty".into(),
            ));
        }
        Ok(())
    }
}
