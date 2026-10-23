//! Non-Unix startup-gate surface with no constructible native authority.

use super::{StartupGateError, StartupGateSpec, ValidatedGateRequestSnapshot};
use std::collections::BTreeMap;
use std::convert::Infallible;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct PinnedControlDir(Infallible);

impl fmt::Debug for PinnedControlDir {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let _ = &self.0;
        formatter.write_str("PinnedControlDir(unsupported)")
    }
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct PinnedRegularFile(Infallible);

impl fmt::Debug for PinnedRegularFile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let _ = &self.0;
        formatter.write_str("PinnedRegularFile(unsupported)")
    }
}

impl PinnedRegularFile {
    pub(crate) fn exact_bytes(&self) -> &[u8] {
        match &self.0 {}
    }

    pub(crate) fn held_bytes_match(&self) -> bool {
        match &self.0 {}
    }
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct PinnedGuestControl(Infallible);

impl fmt::Debug for PinnedGuestControl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let _ = &self.0;
        formatter.write_str("PinnedGuestControl(unsupported)")
    }
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct CapturedControlLocator(Infallible);

impl fmt::Debug for CapturedControlLocator {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let _ = &self.0;
        formatter.write_str("CapturedControlLocator(unsupported)")
    }
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct OrchestratedControlAuthority {
    pub host_parent: PinnedControlDir,
    pub guest_control: PinnedGuestControl,
    #[allow(dead_code)] // Kept only to preserve the unreachable Unix authority shape.
    pub locator: CapturedControlLocator,
    pub request: ValidatedGateRequestSnapshot,
    validated_manifests: Arc<BTreeMap<String, Vec<u8>>>,
}

impl OrchestratedControlAuthority {
    pub(crate) fn validated_manifests(&self) -> &BTreeMap<String, Vec<u8>> {
        self.validated_manifests.as_ref()
    }
}

impl fmt::Debug for OrchestratedControlAuthority {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OrchestratedControlAuthority")
            .field("host_parent", &self.host_parent)
            .field("guest_control", &self.guest_control)
            .finish_non_exhaustive()
    }
}

pub(crate) fn revalidate_mount_source(
    _authority: &OrchestratedControlAuthority,
) -> Result<PathBuf, StartupGateError> {
    Err(StartupGateError::UnsupportedPlatform)
}

pub fn load_startup_gate(
    _control_dir: &Path,
    _timeout: Duration,
) -> Result<StartupGateSpec, StartupGateError> {
    Err(StartupGateError::UnsupportedPlatform)
}

#[derive(Debug, thiserror::Error)]
#[allow(dead_code)] // Non-Unix authority is uninhabited; engine signatures still name both errors.
pub(crate) enum PlanFileError {
    #[error("launch plan already exists or is unsafe")]
    ExistingOrUnsafe,
    #[error("launch plan publication failed")]
    PublicationFailed,
}

impl PinnedControlDir {
    pub(crate) fn existing_launch_plan(&self) -> Result<Option<PinnedRegularFile>, PlanFileError> {
        let _ = &self.0;
        Err(PlanFileError::PublicationFailed)
    }

    pub(crate) fn publish_launch_plan_owned(
        &self,
        _bytes: &[u8],
    ) -> Result<
        (PinnedControlDir, PinnedRegularFile),
        (PlanFileError, Option<(PinnedControlDir, PinnedRegularFile)>),
    > {
        let _ = &self.0;
        Err((PlanFileError::PublicationFailed, None))
    }

    pub(crate) fn verify_pending_launch_plan(
        &self,
        expected: &PinnedRegularFile,
    ) -> Result<(), PlanFileError> {
        let _ = (&self.0, &expected.0);
        Err(PlanFileError::PublicationFailed)
    }

    pub(crate) fn publish_launch_receipt(
        &self,
        _bytes: &[u8],
    ) -> Result<PinnedRegularFile, PlanFileError> {
        let _ = &self.0;
        Err(PlanFileError::PublicationFailed)
    }

    #[allow(dead_code)]
    pub(crate) fn publish_or_verify_cleanup_record(
        &self,
        _bytes: &[u8],
    ) -> Result<PinnedRegularFile, PlanFileError> {
        let _ = &self.0;
        Err(PlanFileError::PublicationFailed)
    }

    pub(crate) fn verify_launch_plan(&self, expected: &PinnedRegularFile) -> bool {
        let _ = (&self.0, &expected.0);
        false
    }

    pub(crate) fn verify_launch_receipt(&self, expected: &PinnedRegularFile) -> bool {
        let _ = (&self.0, &expected.0);
        false
    }

    #[allow(dead_code)]
    pub(crate) fn verify_cleanup_record(&self, expected: &PinnedRegularFile) -> bool {
        let _ = (&self.0, &expected.0);
        false
    }
}
