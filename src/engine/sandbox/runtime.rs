//! `SandboxRuntime` — the sandbox-class `AgentRuntimeEngine` impl.
//!
//! Holds an `Arc<dyn SandboxBackend>`. The concrete driver is invisible
//! outside this module. Platform guards live in the constructors: a user on
//! an unsupported platform gets `BackendUnsupportedOnPlatform` from the
//! constructor and never reaches the backend.

use std::sync::Arc;

use crate::data::session::Session;
use crate::engine::agent_runtime::{
    AgentExecution, AgentFrontend, AgentHandle, AgentHandlePreview, AgentInstance,
    AgentRuntimeEngine, AgentStats, Capabilities, DindSupport, ResolvedAgentOptions,
};
use crate::engine::error::EngineError;
use crate::engine::sandbox::backend::SandboxBackend;
use crate::engine::sandbox::dsbx::DSbxBackend;
use crate::engine::sandbox::options::ResolvedSandboxOptions;

/// Capabilities shared by sandbox-class runtimes: kit-declarative,
/// persistent, workspace-only mounts, private DinD per VM.
static SANDBOX_CAPABILITIES: Capabilities = Capabilities {
    arbitrary_env_vars: false,
    arbitrary_host_mounts: false,
    cpu_limits: false,
    per_resource_stats: false,
    persistent_lifecycle: true,
    kit_declarative: true,
    dind: DindSupport::Always,
    host_paths_visible: false,
    session_label_supported: false,
};

pub struct SandboxRuntime {
    backend: Arc<dyn SandboxBackend>,
}

impl SandboxRuntime {
    /// Construct with the Docker Sandbox (`sbx`) backend.
    ///
    /// Platform guards: Docker Sandboxes are not available on Linux, and not
    /// on Intel Macs. Erroring here (rather than from the first backend
    /// call) gives the user an actionable platform message up front.
    pub fn dsbx() -> Result<Self, EngineError> {
        if cfg!(target_os = "linux") {
            return Err(EngineError::BackendUnsupportedOnPlatform {
                backend: "docker-sbx-experimental".into(),
                platform: "linux — blocked until the Docker Sandboxes virtiofs \
                           file-creation bug (sbx-releases Issue #51) is fixed upstream"
                    .into(),
            });
        }
        if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
            return Err(EngineError::BackendUnsupportedOnPlatform {
                backend: "docker-sbx-experimental".into(),
                platform: "macos (x86_64) — Docker Sandboxes requires Apple Silicon \
                           (arm64). Intel Macs are not supported"
                    .into(),
            });
        }
        Ok(Self {
            backend: Arc::new(DSbxBackend::new()),
        })
    }
}

impl AgentRuntimeEngine for SandboxRuntime {
    fn runtime_name(&self) -> &'static str {
        self.backend.name()
    }

    fn display_name(&self) -> &'static str {
        match self.backend.name() {
            "docker-sbx-experimental" => "Docker Sandboxes (experimental)",
            _ => "Sandbox",
        }
    }

    fn capabilities(&self) -> &Capabilities {
        &SANDBOX_CAPABILITIES
    }

    fn is_available(&self) -> bool {
        // Probes `sbx ls` (per WI 0090): a missing binary and a logged-out
        // session both make the runtime unusable, and `sbx ls` fails for both.
        use std::process::Stdio;
        let child = std::process::Command::new(self.backend.cli_binary())
            .arg("ls")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        match child {
            Ok(child) => crate::engine::container::runtime::wait_with_timeout(
                child,
                std::time::Duration::from_secs(10),
            )
            .map(|s| s.success())
            .unwrap_or(false),
            Err(_) => false,
        }
    }

    fn build(&self, options: ResolvedAgentOptions) -> Result<Box<dyn AgentInstance>, EngineError> {
        match options {
            ResolvedAgentOptions::Sandbox(opts) => {
                Ok(Box::new(SandboxAgentInstance { options: opts }))
            }
            other => Err(EngineError::OptionVariantMismatch {
                runtime: self.runtime_name().to_string(),
                got: other.paradigm(),
            }),
        }
    }

    fn list_running(&self, _session: &Session) -> Result<Vec<AgentHandle>, EngineError> {
        // Sandboxes have no session label; attribution is by name (WI 0090).
        self.backend.list_running()
    }

    fn list_running_all(&self) -> Result<Vec<AgentHandle>, EngineError> {
        self.backend.list_running()
    }

    fn stats(&self, handle: &AgentHandle) -> Result<AgentStats, EngineError> {
        self.backend.stats(handle)
    }

    fn stop(&self, handle: &AgentHandle) -> Result<(), EngineError> {
        self.backend.stop(handle)
    }

    fn exec_args(
        &self,
        agent_id: &str,
        _working_dir: &str,
        entrypoint: &[&str],
        env_vars: &[(&str, &str)],
    ) -> Vec<String> {
        // `sbx exec -it [--env K=V…] <sandbox-name> <entrypoint…>`.
        //
        // `agent_id` carries the deterministic sandbox name for re-attach.
        // Per Phase 0 #3 (Issue #63), the caller passes COLUMNS/LINES through
        // `env_vars` so TUI apps inside the VM see a real terminal size; they
        // are emitted as `--env` like any other variable. The kit's default
        // working directory is the mounted workspace, so `working_dir` needs
        // no translation here.
        let mut args = vec!["exec".to_string(), "-it".to_string()];
        for (k, v) in env_vars {
            args.push("--env".to_string());
            args.push(format!("{k}={v}"));
        }
        args.push(agent_id.to_string());
        args.extend(entrypoint.iter().map(|s| s.to_string()));
        args
    }

    fn cli_binary(&self) -> &'static str {
        self.backend.cli_binary()
    }
}

/// Configured-but-not-running sandbox agent — the sandbox tier's half of the
/// two-step build/run pattern.
struct SandboxAgentInstance {
    options: ResolvedSandboxOptions,
}

impl AgentInstance for SandboxAgentInstance {
    fn handle_preview(&self) -> AgentHandlePreview {
        let name = self
            .options
            .sandbox_name
            .clone()
            .unwrap_or_else(|| self.options.agent_id.clone());
        AgentHandlePreview {
            id: name.clone(),
            name,
            // Sandboxes boot a kit/template rather than a local image; the
            // kit selector is the closest analogue.
            image: self.options.agent_id.clone(),
        }
    }

    fn run_with_frontend(
        self: Box<Self>,
        frontend: Box<dyn AgentFrontend>,
    ) -> Result<AgentExecution, EngineError> {
        // The interactive launch (session config, credential injection, kit
        // selection, PTY-bridged `sbx run`) lives in the dsbx driver.
        super::dsbx::run_interactive(self.options, frontend)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::agent_runtime::{AgentRuntimeEngine, ResolvedAgentOptions};
    use crate::engine::container::options::ResolvedContainerOptions;
    use crate::engine::error::EngineError;

    // ─── Platform guards ──────────────────────────────────────────────────────

    #[test]
    fn dsbx_errors_on_linux() {
        if cfg!(target_os = "linux") {
            match SandboxRuntime::dsbx() {
                Err(EngineError::BackendUnsupportedOnPlatform { backend, platform }) => {
                    assert_eq!(backend, "docker-sbx-experimental");
                    assert!(
                        platform.starts_with("linux"),
                        "platform should name linux, got: {platform}"
                    );
                    assert!(
                        platform.contains("Issue #51"),
                        "platform should explain the upstream blocker, got: {platform}"
                    );
                }
                Err(e) => panic!("expected BackendUnsupportedOnPlatform on linux, got: {e:?}"),
                Ok(_) => panic!("dsbx() must fail on linux"),
            }
        }
    }

    #[test]
    fn dsbx_errors_on_x86_64_macos() {
        if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
            match SandboxRuntime::dsbx() {
                Err(EngineError::BackendUnsupportedOnPlatform { backend, platform }) => {
                    assert_eq!(backend, "docker-sbx-experimental");
                    assert!(
                        platform.contains("macos"),
                        "platform should mention macos, got: {platform}"
                    );
                    assert!(
                        platform.contains("x86_64"),
                        "platform should mention x86_64, got: {platform}"
                    );
                    assert!(
                        platform.contains("Apple Silicon"),
                        "platform should explain the arm64 requirement, got: {platform}"
                    );
                }
                Err(e) => {
                    panic!("expected BackendUnsupportedOnPlatform on x86_64 macos, got: {e:?}")
                }
                Ok(_) => panic!("dsbx() must fail on x86_64 macos"),
            }
        }
    }

    // ─── Option-variant mismatch via SandboxRuntime ───────────────────────────

    /// `SandboxRuntime::build` must reject container-paradigm options with a
    /// clear `OptionVariantMismatch` error on platforms where dsbx is
    /// supported. Skipped via early-return on unsupported platforms.
    #[test]
    fn sandbox_runtime_via_trait_rejects_container_options() {
        let rt = match SandboxRuntime::dsbx() {
            Ok(rt) => rt,
            Err(_) => return, // unsupported platform — platform guard test covers this
        };
        let opts = ResolvedAgentOptions::Container(ResolvedContainerOptions::resolve([]).unwrap());
        match <SandboxRuntime as AgentRuntimeEngine>::build(&rt, opts) {
            Err(EngineError::OptionVariantMismatch { runtime, got }) => {
                assert_eq!(runtime, "docker-sbx-experimental");
                assert_eq!(got, "container");
            }
            Err(e) => panic!("expected OptionVariantMismatch, got: {e:?}"),
            Ok(_) => panic!("expected error, got Ok"),
        }
    }

    // ─── runtime_name and display_name ────────────────────────────────────────

    #[test]
    fn dsbx_runtime_name_and_display_name() {
        // dsbx() errors on unsupported platforms; the guard tests above
        // cover that path.
        if let Ok(rt) = SandboxRuntime::dsbx() {
            assert_eq!(rt.runtime_name(), "docker-sbx-experimental");
            assert!(
                rt.display_name().contains("experimental"),
                "display_name should mention experimental, got: {}",
                rt.display_name()
            );
        }
    }
}
