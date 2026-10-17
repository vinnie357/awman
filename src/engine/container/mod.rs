//! `engine::container` — `ContainerRuntime`, the container-class
//! `AgentRuntimeEngine` impl, and the typed `ContainerOption` enum.
//!
//! The Docker and Apple backends are `pub(super)` and their concrete types
//! are invisible to callers outside this module. All callers go through
//! `ContainerRuntime::build`.
//!
//! This module re-exports only container-paradigm-specific items.
//! Cross-paradigm types (`AgentInstance`, `AgentExecution`, `AgentFrontend`,
//! `AgentHandle`, …) live in `src/engine/agent_runtime/`.

mod apple;
#[cfg(all(test, unix))]
mod apple_initial_inspection_bounds_test;
mod attach_socket;
mod backend;
pub mod background;
pub mod display;
mod docker;
pub(crate) mod gated_launch;
#[cfg(all(test, unix))]
mod gated_launch_actual_barrier_retention_test;
#[cfg(all(test, unix))]
mod gated_launch_gate_rejection_cleanup_test;
#[cfg(test)]
mod gated_launch_native_poll_diagnostic_test;
#[cfg(all(test, unix))]
mod gated_launch_never_created_reconciliation_test;
#[cfg(test)]
mod gated_launch_p2b_test;
#[cfg(test)]
mod gated_launch_p2c_test;
#[cfg(all(test, unix))]
mod gated_launch_publication_acceptance_test;
#[cfg(all(test, unix))]
mod gated_launch_publication_custody_test;
#[cfg(all(test, unix))]
mod gated_launch_retention_acceptance_test;
#[cfg(all(test, unix))]
mod gated_launch_retention_fairness_test;
#[cfg(all(test, unix))]
mod gated_launch_retention_quiescence_test;
pub mod instance;
pub mod io_bridge;
pub mod naming;
#[cfg(test)]
mod native_sdk_execution;
#[cfg(test)]
mod native_sdk_execution_test;
#[cfg(test)]
mod native_sdk_observation_test;
pub mod options;
pub(crate) mod orchestrated_apple;
mod process;
pub mod runtime;
pub mod startup_gate;
pub mod timing;

pub use background::BackgroundContainer;
pub use instance::ContainerId;
pub use naming::generate_container_name;
pub use options::{
    AgentSettings, AutoMode, ContainerName, ContainerOption, CpuLimit, Entrypoint, EnvLiteral,
    EnvVar, ImageRef, MemoryLimit, ModelFlagForm, OverlayPermission, OverlaySpec, PlanMode,
    YoloMode,
};
pub use runtime::ContainerRuntime;

#[cfg(all(test, unix))]
mod native_print_acceptance_test;
#[cfg(all(test, unix))]
mod native_sdk_state_publication_test;

#[cfg(all(test, unix))]
mod native_sdk_stderr_publication_test;

#[cfg(all(test, unix))]
mod io_bridge_stderr_read_test;
