//! Staging and argv wrapping for the fixed startup-gate bootstrap.

use crate::data::startup_gate::{
    revalidate_mount_source, OrchestratedControlAuthority, StartupGateSpec,
};
use crate::engine::container::options::{OverlayPermission, OverlaySpec};
use crate::engine::error::EngineError;
use std::path::PathBuf;
use std::sync::Arc;

pub const CONTAINER_GATE_ROOT: &str = "/.awman/startup-gate";
const BOOTSTRAP: &[u8] = include_bytes!("startup_gate_bootstrap.py");

#[derive(Debug)]
pub struct StartupGateCleanup {
    directory: Option<tempfile::TempDir>,
}

#[derive(Debug)]
pub struct StagedStartupGate {
    pub overlays: Vec<OverlaySpec>,
    pub wrapper_argv: Vec<String>,
    pub cleanup: StartupGateCleanup,
    #[allow(dead_code)]
    // Container launch mounts this after AgentEngine removes its native-launch rejection.
    pub(crate) orchestrated_mount: Option<GuestControlMount>,
}

#[derive(Clone, Debug)]
pub(crate) struct GuestControlMount {
    #[allow(dead_code)] // Real-mount revalidation reads this after native launch is enabled.
    pub controls: Arc<OrchestratedControlAuthority>,
    #[allow(dead_code)] // Real-mount construction reads this after native launch is enabled.
    pub container_path: PathBuf,
}

impl GuestControlMount {
    #[allow(dead_code)] // Container launch calls this before its real mount after native launch is enabled.
    pub(crate) fn revalidated_overlay(&self) -> Result<OverlaySpec, EngineError> {
        let host_path = revalidate_mount_source(&self.controls).map_err(|_| {
            EngineError::Config("orchestrated startup gate control identity changed".into())
        })?;
        Ok(OverlaySpec {
            host_path,
            container_path: self.container_path.clone(),
            permission: OverlayPermission::ReadWrite,
        })
    }
}

impl StagedStartupGate {
    pub fn preserve_python_environment(
        &self,
        values: &std::collections::BTreeMap<String, String>,
    ) -> Result<(), EngineError> {
        let host_root = self
            .overlays
            .first()
            .ok_or_else(|| EngineError::Config("startup gate bootstrap overlay missing".into()))?
            .host_path
            .clone();
        let path = host_root.join("original-python-env.json");
        let raw = serde_json::to_vec(values).map_err(|error| {
            EngineError::Config(format!("startup gate environment snapshot: {error}"))
        })?;
        std::fs::write(&path, raw).map_err(|source| EngineError::Io {
            path: path.clone(),
            source,
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .map_err(|source| EngineError::Io { path, source })?;
        }
        Ok(())
    }
}

pub fn stage_startup_gate(spec: &StartupGateSpec) -> Result<StagedStartupGate, EngineError> {
    let (request_bytes, validated_manifests, orchestrated_controls) =
        if let Some((controls, _identity)) = spec.control.orchestrated_parts() {
            revalidate_mount_source(controls).map_err(|_| {
                EngineError::Config("orchestrated startup gate changed after validation".into())
            })?;
            if controls.request.request.as_ref() != &spec.request
                || controls.request.request_digest != spec.request_digest
                || controls.validated_manifests() != &spec.validated_manifests
            {
                return Err(EngineError::Config(
                    "startup gate request changed after validation".into(),
                ));
            }
            (
                controls.request.request_bytes().to_vec(),
                controls.validated_manifests().clone(),
                Some(Arc::clone(controls)),
            )
        } else {
            let control_dir = spec.control.legacy_control_dir().ok_or_else(|| {
                EngineError::Config("startup gate control layout unavailable".into())
            })?;
            let approved = crate::data::startup_gate::load_startup_gate(control_dir, spec.timeout)
                .map_err(|error| {
                    EngineError::Config(format!("startup gate changed after validation: {error}"))
                })?;
            if approved.request != spec.request
                || approved.request_digest != spec.request_digest
                || approved.control != spec.control
                || approved.validated_manifests != spec.validated_manifests
            {
                return Err(EngineError::Config(
                    "startup gate request changed after validation".into(),
                ));
            }
            let bytes = serde_json::to_vec(&approved.request).map_err(|error| {
                EngineError::Config(format!("startup gate request snapshot: {error}"))
            })?;
            (bytes, approved.validated_manifests, None)
        };

    let directory = tempfile::Builder::new()
        .prefix("awman-startup-gate-")
        .tempdir()
        .map_err(|source| EngineError::Io {
            path: std::env::temp_dir(),
            source,
        })?;
    let script = directory.path().join("bootstrap.py");
    std::fs::write(&script, BOOTSTRAP).map_err(|source| EngineError::Io {
        path: script.clone(),
        source,
    })?;
    let request_path = directory.path().join("request.json");
    std::fs::write(&request_path, request_bytes).map_err(|source| EngineError::Io {
        path: request_path.clone(),
        source,
    })?;
    for (name, bytes) in &validated_manifests {
        let target_path = directory.path().join(name);
        std::fs::write(&target_path, bytes).map_err(|source| EngineError::Io {
            path: target_path.clone(),
            source,
        })?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
            .map_err(|source| EngineError::Io {
                path: directory.path().to_path_buf(),
                source,
            })?;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o600)).map_err(
            |source| EngineError::Io {
                path: script.clone(),
                source,
            },
        )?;
        std::fs::set_permissions(&request_path, std::fs::Permissions::from_mode(0o600)).map_err(
            |source| EngineError::Io {
                path: request_path.clone(),
                source,
            },
        )?;
        for binding in &spec.request.bindings {
            let path = directory.path().join(&binding.manifest_file);
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .map_err(|source| EngineError::Io { path, source })?;
        }
    }

    let control_path = PathBuf::from(format!("{CONTAINER_GATE_ROOT}/control"));
    let mut overlays = vec![OverlaySpec {
        host_path: directory.path().to_path_buf(),
        container_path: PathBuf::from(format!("{CONTAINER_GATE_ROOT}/bin")),
        permission: OverlayPermission::ReadOnly,
    }];
    let orchestrated_mount = if let Some(controls) = orchestrated_controls {
        Some(GuestControlMount {
            controls,
            container_path: control_path,
        })
    } else {
        let host_path = spec
            .control
            .legacy_control_dir()
            .ok_or_else(|| EngineError::Config("legacy startup gate path unavailable".into()))?
            .to_path_buf();
        overlays.push(OverlaySpec {
            host_path,
            container_path: control_path,
            permission: OverlayPermission::ReadWrite,
        });
        None
    };
    let wrapper_argv = vec![
        "/usr/bin/python3".into(),
        "-I".into(),
        "-S".into(),
        format!("{CONTAINER_GATE_ROOT}/bin/bootstrap.py"),
        format!("{CONTAINER_GATE_ROOT}/control"),
        spec.timeout.as_secs().to_string(),
    ];
    Ok(StagedStartupGate {
        overlays,
        wrapper_argv,
        cleanup: StartupGateCleanup {
            directory: Some(directory),
        },
        orchestrated_mount,
    })
}

pub fn wrap_entrypoint(
    staged: &StagedStartupGate,
    original: &[String],
) -> Result<Vec<String>, EngineError> {
    if original.is_empty() {
        return Err(EngineError::MissingRequiredOption(
            "startup gate original entrypoint".into(),
        ));
    }
    let mut argv = staged.wrapper_argv.clone();
    argv.push("--".into());
    argv.extend_from_slice(original);
    Ok(argv)
}

impl Drop for StartupGateCleanup {
    fn drop(&mut self) {
        let _ = self.directory.take();
    }
}

#[cfg(test)]
#[path = "startup_gate_native_p2_test.rs"]
mod native_p2;
