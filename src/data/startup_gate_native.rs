//! Portable value contract for startup-gate loading and held authority.

use crate::data::container::ContainerName;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

#[cfg(unix)]
#[path = "startup_gate_native_unix.rs"]
mod platform;

#[cfg(not(unix))]
#[path = "startup_gate_native_unsupported.rs"]
mod platform;

pub use platform::load_startup_gate;
pub(crate) use platform::{
    revalidate_mount_source, OrchestratedControlAuthority, PinnedControlDir, PinnedRegularFile,
    PlanFileError,
};

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StartupGateRequest {
    pub version: u32,
    pub bindings: Vec<StartupGateBinding>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StartupGateBinding {
    pub id: String,
    pub workspace_path: String,
    pub manifest_id: String,
    pub manifest_file: String,
    pub access: StartupGateAccess,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum StartupGateAccess {
    ReadOnly,
    ReadWrite,
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct ValidatedGateRequestSnapshot {
    pub request: Arc<StartupGateRequest>,
    pub request_digest: [u8; 32],
    request_bytes: Arc<Vec<u8>>,
}

impl ValidatedGateRequestSnapshot {
    pub(crate) fn request_bytes(&self) -> &[u8] {
        self.request_bytes.as_slice()
    }
}

impl fmt::Debug for ValidatedGateRequestSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ValidatedGateRequestSnapshot")
            .field("request", &self.request)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct GatedLaunchIdentity {
    pub container_name: ContainerName,
    pub token_digest: [u8; 32],
    pub request_digest: [u8; 32],
}

impl fmt::Debug for GatedLaunchIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GatedLaunchIdentity")
            .field("container_name", &self.container_name)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct StartupGateControlLayout(StartupGateControlLayoutInner);

#[derive(Clone, Eq, PartialEq)]
enum StartupGateControlLayoutInner {
    Legacy {
        control_dir: PathBuf,
    },
    #[cfg_attr(not(unix), allow(dead_code))]
    Orchestrated {
        controls: Arc<OrchestratedControlAuthority>,
        identity: GatedLaunchIdentity,
    },
}

impl StartupGateControlLayout {
    pub fn legacy(control_dir: PathBuf) -> Self {
        Self(StartupGateControlLayoutInner::Legacy { control_dir })
    }

    #[cfg(unix)]
    pub(crate) fn orchestrated(
        controls: Arc<OrchestratedControlAuthority>,
        identity: GatedLaunchIdentity,
    ) -> Self {
        Self(StartupGateControlLayoutInner::Orchestrated { controls, identity })
    }

    pub(crate) fn legacy_control_dir(&self) -> Option<&Path> {
        match &self.0 {
            StartupGateControlLayoutInner::Legacy { control_dir } => Some(control_dir),
            StartupGateControlLayoutInner::Orchestrated { .. } => None,
        }
    }

    pub(crate) fn orchestrated_parts(
        &self,
    ) -> Option<(&Arc<OrchestratedControlAuthority>, &GatedLaunchIdentity)> {
        match &self.0 {
            StartupGateControlLayoutInner::Legacy { .. } => None,
            StartupGateControlLayoutInner::Orchestrated { controls, identity } => {
                Some((controls, identity))
            }
        }
    }
}

impl fmt::Debug for StartupGateControlLayout {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            StartupGateControlLayoutInner::Legacy { control_dir } => formatter
                .debug_struct("Legacy")
                .field("control_dir", control_dir)
                .finish(),
            StartupGateControlLayoutInner::Orchestrated { controls, identity } => formatter
                .debug_struct("Orchestrated")
                .field("controls", controls)
                .field("identity", identity)
                .finish(),
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct StartupGateSpec {
    pub control: StartupGateControlLayout,
    pub request: StartupGateRequest,
    pub request_digest: [u8; 32],
    pub timeout: Duration,
    pub validated_manifests: BTreeMap<String, Vec<u8>>,
}

impl fmt::Debug for StartupGateSpec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StartupGateSpec")
            .field("control", &self.control)
            .field("request", &self.request)
            .field("timeout", &self.timeout)
            .field("validated_manifests", &self.validated_manifests.keys())
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StartupGateError {
    #[error("startup gates are unsupported on this platform")]
    UnsupportedPlatform,
    #[error("invalid startup gate: {0}")]
    Invalid(String),
    #[error("startup gate I/O at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid orchestrated startup gate: {0}")]
    OrchestratedInvalid(&'static str),
    #[error("invalid launch intention at {line}:{column}")]
    InvalidIntent { line: usize, column: usize },
    #[error("orchestrated startup gate I/O failure")]
    OrchestratedIo(#[source] std::io::Error),
}
