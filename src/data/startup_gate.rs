//! Validated on-disk contract for the opt-in pre-agent startup gate.

#[path = "startup_gate_native.rs"]
mod native;
#[cfg(all(test, unix))]
pub(crate) use native::NativeFileIdentity;

#[cfg(all(test, unix))]
pub(crate) use native::{
    after_auth_release_noreplace_rename, after_launch_plan_native_publication,
    arm_auth_release_post_rename_sync_failure, arm_launch_plan_post_rename_sync_failure,
    AuthReleaseSyncFaultHandle,
};
pub use native::{
    load_startup_gate, StartupGateAccess, StartupGateBinding, StartupGateControlLayout,
    StartupGateError, StartupGateRequest, StartupGateSpec,
};
#[allow(unused_imports)] // Packet 1A exposes the pinned type to later engine packets.
pub(crate) use native::{
    revalidate_mount_source, GatedLaunchIdentity, OrchestratedControlAuthority, PinnedControlDir,
    PinnedRegularFile, PlanFileError,
};
