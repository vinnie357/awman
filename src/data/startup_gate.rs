//! Validated on-disk contract for the opt-in pre-agent startup gate.

#[path = "startup_gate_native.rs"]
mod native;

pub use native::{
    load_startup_gate, StartupGateAccess, StartupGateBinding, StartupGateControlLayout,
    StartupGateError, StartupGateRequest, StartupGateSpec,
};
#[allow(unused_imports)] // Packet 1A exposes the pinned type to later engine packets.
pub(crate) use native::{
    revalidate_mount_source, GatedLaunchIdentity, OrchestratedControlAuthority, PinnedControlDir,
    PinnedRegularFile, PlanFileError,
};
