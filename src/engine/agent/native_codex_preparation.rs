//! First-owner contained auth; no main-launch or completion authority is exposed.
use super::{agent_matrix::NativePrintRequest, AgentEngine};
use crate::data::fs::data_paths::DataPaths;
use crate::data::fs::native_codex_auth::{
    AuthReleaseDestination, CodexAuthLaunchFiles, GUEST_CODEX_HOME,
};
use crate::data::fs::native_sdk_state::{
    NativeSdkProvider, NativeSdkRunLease, NativeSdkState, NativeSdkStateKey,
};
use crate::data::session::Session;
use crate::data::startup_gate::StartupGateAccess;
#[cfg(test)]
use crate::engine::agent_runtime::CancelHandle;
use crate::engine::agent_runtime::{AgentExecution, AgentFrontend};
use crate::engine::container::gated_launch::{
    revalidate_post_spawn, DurableLaunchPlan, LaunchRetentionReason, LaunchRetentionTicket,
    ProviderKind,
};
use crate::engine::container::options::{
    ContainerOption, Entrypoint, EnvLiteral, ImageRef, OverlayPermission, OverlaySpec,
    ResolvedContainerOptions,
};
use crate::engine::container::ContainerRuntime;
use crate::engine::error::EngineError;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

struct Preparation {
    lease: NativeSdkRunLease,
    runtime: Weak<ContainerRuntime>,
    data: DataPaths,
    model: String,
    files: Option<CodexAuthLaunchFiles>,
    #[cfg(all(test, unix))]
    observer: Option<Arc<super::native_codex_auth_preparation_test::AuthObserver>>,
}
pub(crate) struct NativeCodexPreparation {
    preparation: Box<Preparation>,
}
pub(crate) struct StoppedPreparation {
    _preparation: Box<Preparation>,
}
pub(crate) struct RetryableAuthPreparation {
    _preparation: Box<Preparation>,
}
pub(crate) struct RetiredAuth {
    _preparation: Box<Preparation>,
}
// The existing lifecycle/registry or detached fallback also owns this Arc.
// This result does not permit extraction, retry or a claim of non-start.
pub(crate) struct AuthCustodyHeld {
    _custody: Arc<AuthCustody>,
}
pub(crate) enum PreparationError {
    Rejected(EngineError),
    Activated {
        owner: StoppedPreparation,
        error: EngineError,
    },
}
pub(crate) enum AuthStartError {
    CustodyHeld {
        owner: AuthCustodyHeld,
        error: EngineError,
    },
    NotStarted {
        preparation: NativeCodexPreparation,
        error: EngineError,
    },
    Retained {
        ticket: LaunchRetentionTicket,
        error: EngineError,
    },
}
pub(crate) enum AuthRetirement {
    CustodyHeld {
        owner: AuthCustodyHeld,
        error: EngineError,
    },
    Authenticated(RetiredAuth),
    Retryable(RetryableAuthPreparation),
    Stopped {
        owner: StoppedPreparation,
        error: EngineError,
    },
    Retained {
        ticket: LaunchRetentionTicket,
        error: EngineError,
    },
}

// Arcs share custody; none clones the original strict lease. The resource
// bundle and returned execution retain this owner until retirement/recovery.
pub(crate) struct AuthCustody {
    preparation: Mutex<Option<Box<Preparation>>>,
    release: Mutex<Option<AuthReleaseCapability>>,
    release_failed: AtomicBool,
    ticket: Mutex<Option<LaunchRetentionTicket>>,
    // Set false when a protected plan exists; true again only after actual
    // never-created cleanup resolves. Absence of a ticket is not this proof.
    pub(crate) no_child_and_plan: AtomicBool,
    pub(crate) reader_failed: AtomicBool,
    pub(crate) retired: AtomicBool,
    cancelled: AtomicBool,
}
// ADR0002 makes preparation the caller for its own SDK-only auth launch.
// No external/main launch or cloneable options can mint this capability.
struct AuthReleaseCapability {
    destination: AuthReleaseDestination,
    container_name: String,
    token_digest: [u8; 32],
    request_digest: [u8; 32],
}
impl AuthReleaseCapability {
    fn release_after_verified_receipt(
        self,
        owner: &AuthCustody,
        plan: &DurableLaunchPlan,
        nonce: &str,
        cancelled: &AtomicBool,
    ) -> Result<(), LaunchRetentionReason> {
        let changed = LaunchRetentionReason::PostSpawnControlChanged;
        // Consume before validation: even failure cannot recreate permission
        // after the guest has consumed the fixed release filename.
        if cancelled.load(Ordering::Acquire) {
            return Err(LaunchRetentionReason::ChildStateUnknown);
        }
        if !self.destination.matches_controls(&plan.controls)
            || plan.key.provider != ProviderKind::AppleContainers
            || plan.key.container_name.as_str() != self.container_name
            || plan.key.token_digest != self.token_digest
            || plan.controls.request.request_digest != self.request_digest
        {
            return Err(changed);
        }
        let request = &plan.controls.request.request;
        if request.version != 2
            || request.guest_cwd.as_deref() != Some("/workspace")
            || request.bindings.len() != 1
            || request.bindings[0].workspace_path != GUEST_CODEX_HOME
            || request.bindings[0].access != StartupGateAccess::ReadWrite
        {
            return Err(changed);
        }
        owner.validate().map_err(|_| changed)?;
        revalidate_post_spawn(plan).map_err(|_| changed)?;
        if cancelled.load(Ordering::Acquire) {
            return Err(LaunchRetentionReason::ChildStateUnknown);
        }
        self.destination.publish(nonce).map_err(|_| changed)?;
        owner.validate().map_err(|_| changed)?;
        revalidate_post_spawn(plan).map_err(|_| changed)?;
        if cancelled.load(Ordering::Acquire) {
            return Err(LaunchRetentionReason::ChildStateUnknown);
        }
        Ok(())
    }
}
impl AuthCustody {
    // The sole production take/validate/publish dispatch; all attempts use it.
    pub(crate) fn release_after_verified_receipt(
        &self,
        plan: &DurableLaunchPlan,
        nonce: &str,
        cancelled: &AtomicBool,
    ) -> Result<(), LaunchRetentionReason> {
        let capability = {
            self.release
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .take()
        };
        #[cfg(all(test, unix))]
        if let Some(observer) = self.observer() {
            observer.observe_auth_release_take(capability.is_some());
        }
        let capability = capability.ok_or(LaunchRetentionReason::PostSpawnControlChanged)?;
        let result = capability.release_after_verified_receipt(self, plan, nonce, cancelled);
        if result.is_err() {
            self.release_failed.store(true, Ordering::Release);
        }
        result
    }

    pub(crate) fn auth_release_failed(&self) -> bool {
        self.release_failed.load(Ordering::Acquire)
    }

    pub(crate) fn retained(&self, ticket: LaunchRetentionTicket) {
        *self.ticket.lock().unwrap_or_else(|p| p.into_inner()) = Some(ticket);
    }
    pub(crate) fn validate(&self) -> Result<(), EngineError> {
        let guard = self.preparation.lock().unwrap_or_else(|p| p.into_inner());
        let prep = guard
            .as_ref()
            .ok_or_else(|| EngineError::Config("auth custody unavailable".into()))?;
        prep.lease
            .validate_request(NativeSdkProvider::Codex, &prep.model, None)?;
        Ok(())
    }
    fn take(&self) -> Box<Preparation> {
        self.preparation
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
            .expect("single consuming auth ownership transfer")
    }
    pub(crate) fn ticket(&self) -> Option<LaunchRetentionTicket> {
        *self.ticket.lock().unwrap_or_else(|p| p.into_inner())
    }
    #[cfg(all(test, unix))]
    pub(crate) fn observer(
        &self,
    ) -> Option<Arc<super::native_codex_auth_preparation_test::AuthObserver>> {
        self.preparation
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .and_then(|p| p.observer.clone())
    }
}
pub(crate) struct NativeCodexAuthExecution {
    execution: AgentExecution,
    custody: Arc<AuthCustody>,
}
impl AgentEngine {
    #[cfg(all(test, unix))]
    pub(crate) fn with_native_codex_auth_test_observer(
        mut self,
        observer: Arc<super::native_codex_auth_preparation_test::AuthObserver>,
    ) -> Self {
        self.native_codex_auth_observer = Some(observer);
        self
    }
    pub(crate) fn prepare_native_codex(
        &self,
        session: &Session,
        request: NativePrintRequest,
    ) -> Result<NativeCodexPreparation, PreparationError> {
        let model = request
            .fresh_codex_model()
            .map_err(PreparationError::Rejected)?
            .to_owned();
        for key in [
            "CODEX_HOME",
            "CODEX_SQLITE_HOME",
            "CODEX_API_KEY",
            "CODEX_ACCESS_TOKEN",
            "OPENAI_FEDERATION_RULE_ID",
            "OPENAI_IDENTITY_TOKEN_FILE",
            "OPENAI_WORKLOAD_IDENTITY_CONTEXT",
            "CODEX_REFRESH_TOKEN_URL_OVERRIDE",
            "CODEX_REVOKE_TOKEN_URL_OVERRIDE",
            "CODEX_APP_SERVER_LOGIN_CLIENT_ID",
            "OPENAI_API_KEY",
        ] {
            if session.env().get(key).is_some() {
                return Err(PreparationError::Rejected(EngineError::Config(format!(
                    "unqualified native Codex selector: {key}"
                ))));
            }
        }
        if self.container_runtime.runtime_name() != "apple-containers" {
            return Err(PreparationError::Rejected(EngineError::Config(
                "contained Codex auth requires Apple".into(),
            )));
        }
        // The first packet qualifies no real guest image. Only the external
        // test profile/provider is substituted; ownership and launch are real.
        let qualified = false;
        #[cfg(all(test, unix))]
        let qualified = qualified || self.native_codex_auth_observer.is_some();
        if !qualified {
            return Err(PreparationError::Rejected(EngineError::Config(
                "native Codex auth profile is not qualified".into(),
            )));
        }
        let data =
            DataPaths::from_env(session.env()).map_err(|e| PreparationError::Rejected(e.into()))?;
        let state = NativeSdkState::new(&data).map_err(|e| PreparationError::Rejected(e.into()))?;
        let key = NativeSdkStateKey::new(session.git_root(), NativeSdkProvider::Codex, &model)
            .map_err(|e| PreparationError::Rejected(e.into()))?;
        let lease = state
            .acquire_first_run(&key)
            .map_err(|e| PreparationError::Rejected(e.into()))?;
        let preparation = Box::new(Preparation {
            lease,
            runtime: Arc::downgrade(&self.container_runtime),
            data,
            model,
            files: None,
            #[cfg(all(test, unix))]
            observer: self.native_codex_auth_observer.clone(),
        });
        #[cfg(all(test, unix))]
        if let Some(observer) = &preparation.observer {
            observer.before_owner_material(preparation.lease.state_path());
        }
        if let Err(error) = preparation.lease.create_codex_auth_config() {
            return Err(PreparationError::Activated {
                owner: StoppedPreparation {
                    _preparation: preparation,
                },
                error: error.into(),
            });
        }
        Ok(NativeCodexPreparation { preparation })
    }
}
impl NativeCodexPreparation {
    pub(crate) fn start_auth(
        self,
        user: Box<dyn AgentFrontend>,
    ) -> Result<NativeCodexAuthExecution, AuthStartError> {
        let mut prep = self.preparation;
        let Some(runtime) = prep.runtime.upgrade() else {
            return Err(AuthStartError::NotStarted {
                preparation: NativeCodexPreparation { preparation: prep },
                error: EngineError::Config("native auth runtime is unavailable".into()),
            });
        };
        let mut release = None;
        let options = (|| -> Result<_, EngineError> {
            prep.lease
                .validate_request(NativeSdkProvider::Codex, &prep.model, None)?;
            prep.files = Some(CodexAuthLaunchFiles::new(prep.data.root())?);
            #[cfg(all(test, unix))]
            if let (Some(observer), Some(files)) = (&prep.observer, &prep.files) {
                observer.before_protected_launch_plan(files.controls_parent_for_test());
            }
            let gate = prep.files.as_ref().expect("created controls").gate.clone();
            let (_, identity) = gate
                .control
                .orchestrated_parts()
                .ok_or_else(|| EngineError::Config("auth requires protected controls".into()))?;
            let name = identity.container_name.clone();
            let digest = identity
                .token_digest
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            let mut options = ResolvedContainerOptions::resolve([
                ContainerOption::Image(ImageRef::new("awman-native-codex-qualified")),
                ContainerOption::Entrypoint(Entrypoint(vec![
                    "codex".into(),
                    "login".into(),
                    "--device-auth".into(),
                ])),
                ContainerOption::Name(name),
                ContainerOption::Label {
                    key: "dev.awman.orchestrator-launch".into(),
                    value: digest,
                },
                ContainerOption::NativePrint(true),
                ContainerOption::EnvLiteral(EnvLiteral {
                    key: "CODEX_HOME".into(),
                    value: GUEST_CODEX_HOME.into(),
                }),
                ContainerOption::Overlay(OverlaySpec {
                    host_path: prep.lease.state_path().to_owned(),
                    container_path: GUEST_CODEX_HOME.into(),
                    permission: OverlayPermission::ReadWrite,
                }),
            ])
            .map_err(|error| EngineError::Config(error.to_string()))?;
            options.startup_gate = Some(Box::new(gate));
            options.startup_gate_trusted_template = true;
            options.remove_on_exit = false;
            let (_, identity) = options
                .startup_gate
                .as_ref()
                .expect("auth gate")
                .control
                .orchestrated_parts()
                .ok_or_else(|| EngineError::Config("auth controls unavailable".into()))?;
            let destination = prep
                .files
                .as_mut()
                .expect("created controls")
                .take_release_destination()
                .ok_or_else(|| {
                    EngineError::Config("auth release destination already consumed".into())
                })?;
            release = Some(AuthReleaseCapability {
                destination,
                container_name: identity.container_name.as_str().to_owned(),
                token_digest: identity.token_digest,
                request_digest: identity.request_digest,
            });
            Ok(options)
        })();
        let options = match options {
            Ok(options) => options,
            Err(error) => {
                return Err(AuthStartError::NotStarted {
                    preparation: NativeCodexPreparation { preparation: prep },
                    error,
                })
            }
        };
        let custody = Arc::new(AuthCustody {
            preparation: Mutex::new(Some(prep)),
            release: Mutex::new(release),
            release_failed: AtomicBool::new(false),
            ticket: Mutex::new(None),
            no_child_and_plan: AtomicBool::new(true),
            reader_failed: AtomicBool::new(false),
            retired: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
        });
        let result = runtime
            .build_native_codex_auth(options, Arc::clone(&custody))
            .and_then(|instance| instance.run_with_frontend(user));
        match result {
            Ok(execution) => Ok(NativeCodexAuthExecution { execution, custody }),
            Err(error) => match custody.ticket() {
                Some(ticket) => Err(AuthStartError::Retained { ticket, error }),
                None if custody.no_child_and_plan.load(Ordering::Acquire) => {
                    Err(AuthStartError::NotStarted {
                        preparation: NativeCodexPreparation {
                            preparation: custody.take(),
                        },
                        error,
                    })
                }
                None => Err(AuthStartError::CustodyHeld {
                    owner: AuthCustodyHeld { _custody: custody },
                    error,
                }),
            },
        }
    }
}
impl RetryableAuthPreparation {
    #[cfg(test)]
    pub(crate) fn retry_initial_auth(
        self,
        user: Box<dyn AgentFrontend>,
    ) -> Result<NativeCodexAuthExecution, AuthStartError> {
        NativeCodexPreparation {
            preparation: self._preparation,
        }
        .start_auth(user)
    }
}
impl NativeCodexAuthExecution {
    #[cfg(test)]
    pub(crate) fn cancel_handle(&self) -> CancelHandle {
        let handle = self
            .execution
            .cancel_handle()
            .expect("managed auth has a real cancel handle");
        let custody = Arc::clone(&self.custody);
        #[cfg(all(test, unix))]
        let observer = custody.observer();
        CancelHandle::new(move || {
            let result = handle.cancel();
            #[cfg(all(test, unix))]
            if let Some(observer) = &observer {
                observer.after_actual_auth_cancel_return();
            }
            result?;
            custody.cancelled.store(true, Ordering::Release);
            Ok(())
        })
    }
    pub(crate) async fn wait_auth_retired(mut self) -> AuthRetirement {
        let result = self.execution.wait().await;
        if let Some(ticket) = self.custody.ticket() {
            return AuthRetirement::Retained {
                ticket,
                error: EngineError::Container("auth retained for recovery".into()),
            };
        }
        if !self.custody.retired.load(Ordering::Acquire) {
            return AuthRetirement::CustodyHeld {
                owner: AuthCustodyHeld {
                    _custody: self.custody,
                },
                error: EngineError::Container("auth retirement remains unknown".into()),
            };
        }
        let preparation = self.custody.take();
        if self.custody.cancelled.load(Ordering::Acquire) {
            AuthRetirement::Retryable(RetryableAuthPreparation {
                _preparation: preparation,
            })
        } else if matches!(result, Ok(ref info) if info.exit_code == 0) {
            AuthRetirement::Authenticated(RetiredAuth {
                _preparation: preparation,
            })
        } else {
            AuthRetirement::Stopped {
                owner: StoppedPreparation {
                    _preparation: preparation,
                },
                error: EngineError::Container(
                    "device authentication did not establish qualified success".into(),
                ),
            }
        }
    }
}
