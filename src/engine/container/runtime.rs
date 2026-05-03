//! `ContainerRuntime` — the typed factory for `ContainerInstance` builds.
//!
//! Holds a `Box<dyn ContainerBackend>` chosen by `detect`. The concrete
//! backend is invisible outside this module.

use crate::data::config::global::GlobalConfig;
use crate::data::session::{ContainerHandle, Session};
use crate::engine::container::apple::AppleBackend;
use crate::engine::container::backend::ContainerBackend;
use crate::engine::container::docker::DockerBackend;
use crate::engine::container::instance::{ContainerInstance, ContainerStats};
use crate::engine::container::options::{ContainerOption, ResolvedContainerOptions};
use crate::engine::error::EngineError;

pub struct ContainerRuntime {
    backend: Box<dyn ContainerBackend>,
}

impl ContainerRuntime {
    /// Inspect `global_config` and the host environment to select the correct
    /// backend (Docker by default, Apple Containers when configured + macOS).
    pub fn detect(global_config: &GlobalConfig) -> Result<Self, EngineError> {
        let runtime_name = global_config
            .runtime
            .as_deref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty());
        let chosen = match runtime_name {
            Some("docker") | None => Backend::Docker,
            Some("apple-containers") => {
                if cfg!(target_os = "macos") {
                    Backend::Apple
                } else {
                    return Err(EngineError::BackendUnsupportedOnPlatform {
                        backend: "apple-containers".into(),
                        platform: std::env::consts::OS.into(),
                    });
                }
            }
            Some(other) => {
                return Err(EngineError::Config(format!(
                    "unknown runtime '{other}'; supported values are 'docker' and 'apple-containers'"
                )));
            }
        };
        let backend: Box<dyn ContainerBackend> = match chosen {
            Backend::Docker => Box::new(DockerBackend::new()),
            Backend::Apple => Box::new(AppleBackend::new()),
        };
        Ok(Self { backend })
    }

    /// Construct directly with a Docker backend (escape hatch for tests
    /// and code paths that have already resolved the backend).
    pub fn docker() -> Self {
        Self {
            backend: Box::new(DockerBackend::new()),
        }
    }

    /// Static name of the chosen backend (e.g. `"docker"`).
    pub fn runtime_name(&self) -> &'static str {
        self.backend.name()
    }

    /// Build a fully-configured `ContainerInstance` from the given options.
    pub fn build(
        &self,
        options: impl IntoIterator<Item = ContainerOption>,
    ) -> Result<Box<dyn ContainerInstance>, EngineError> {
        let resolved = ResolvedContainerOptions::resolve(options).map_err(|e| match e {
            crate::engine::container::options::ResolveError::Conflict(msg) => {
                EngineError::ConflictingOptions(msg)
            }
        })?;
        self.backend.build(resolved)
    }

    pub fn list_running(&self, session: &Session) -> Result<Vec<ContainerHandle>, EngineError> {
        self.backend.list_running(session)
    }

    /// Shell out to the underlying CLI to build a container image. Streams
    /// stdout+stderr line-by-line through `on_line`. Returns an error when the
    /// build fails.
    pub fn build_image(
        &self,
        tag: &str,
        dockerfile: &std::path::Path,
        context: &std::path::Path,
        no_cache: bool,
        on_line: &mut dyn FnMut(&str),
    ) -> Result<(), EngineError> {
        use std::io::{BufRead, BufReader};
        use std::process::{Command, Stdio};
        let cli = self.backend.name();
        // Both "docker" and "container" share the same `build` argv shape.
        let cli_bin = match cli {
            "apple-containers" => "container",
            _ => "docker",
        };
        let mut args: Vec<String> = vec!["build".into()];
        if no_cache {
            args.push("--no-cache".into());
        }
        args.extend([
            "-t".into(),
            tag.to_string(),
            "-f".into(),
            dockerfile.display().to_string(),
            context.display().to_string(),
        ]);
        let mut child = Command::new(cli_bin)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| EngineError::Container(format!("spawn {cli_bin} build: {e}")))?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        // Combine stdout + stderr into a single sequenced stream by spawning two
        // threads that funnel into a channel.
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let tx_out = tx.clone();
        let stdout_handle = std::thread::spawn(move || {
            if let Some(out) = stdout {
                let r = BufReader::new(out);
                for line in r.lines().map_while(Result::ok) {
                    let _ = tx_out.send(line);
                }
            }
        });
        let stderr_handle = std::thread::spawn(move || {
            if let Some(err) = stderr {
                let r = BufReader::new(err);
                for line in r.lines().map_while(Result::ok) {
                    let _ = tx.send(line);
                }
            }
        });
        for line in rx {
            on_line(&line);
        }
        let _ = stdout_handle.join();
        let _ = stderr_handle.join();
        let status = child
            .wait()
            .map_err(|e| EngineError::Container(format!("wait {cli_bin} build: {e}")))?;
        if !status.success() {
            return Err(EngineError::ImageBuildExitNonzero {
                tag: tag.to_string(),
                exit_code: status.code().unwrap_or(-1),
            });
        }
        Ok(())
    }

    /// Best-effort check whether an image tag exists locally on the runtime.
    pub fn image_exists(&self, tag: &str) -> bool {
        use std::process::{Command, Stdio};
        let cli_bin = match self.backend.name() {
            "apple-containers" => "container",
            _ => "docker",
        };
        Command::new(cli_bin)
            .args(["image", "inspect", tag])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    pub fn stats(&self, handle: &ContainerHandle) -> Result<ContainerStats, EngineError> {
        self.backend.stats(handle)
    }

    pub fn stop(&self, handle: &ContainerHandle) -> Result<(), EngineError> {
        self.backend.stop(handle)
    }
}

enum Backend {
    Docker,
    Apple,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_default_picks_docker() {
        let cfg = GlobalConfig::default();
        let rt = ContainerRuntime::detect(&cfg).unwrap();
        assert_eq!(rt.runtime_name(), "docker");
    }

    #[test]
    fn detect_apple_on_non_mac_errors() {
        let cfg = GlobalConfig {
            runtime: Some("apple-containers".into()),
            ..Default::default()
        };
        let res = ContainerRuntime::detect(&cfg);
        if cfg!(target_os = "macos") {
            assert!(res.is_ok());
        } else {
            match res {
                Err(EngineError::BackendUnsupportedOnPlatform { .. }) => {}
                Err(e) => panic!("expected BackendUnsupportedOnPlatform, got: {e:?}"),
                Ok(_) => panic!("expected error on non-macOS"),
            }
        }
    }

    #[test]
    fn detect_unknown_runtime_is_hard_error() {
        let cfg = GlobalConfig {
            runtime: Some("blarg".into()),
            ..Default::default()
        };
        match ContainerRuntime::detect(&cfg) {
            Err(EngineError::Config(msg)) => {
                assert!(msg.contains("blarg"), "error message should name the bad value");
            }
            Ok(_) => panic!("expected Config error for unknown runtime, got Ok"),
            Err(e) => panic!("expected Config error for unknown runtime, got Err({e:?})"),
        }
    }

    #[test]
    fn build_requires_image_option() {
        let rt = ContainerRuntime::docker();
        match rt.build([]) {
            Err(EngineError::MissingRequiredOption(opt)) => {
                assert_eq!(opt, "Image");
            }
            Err(e) => panic!("expected MissingRequiredOption, got: {e:?}"),
            Ok(_) => panic!("expected error from missing Image option"),
        }
    }
}
