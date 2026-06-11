//! `engine::sandbox` — `SandboxRuntime`, the sandbox-class
//! `AgentRuntimeEngine` impl for microVM-per-session runtimes.
//!
//! The concrete sandbox drivers are `pub(super)`-style internals: callers
//! outside this module see only `SandboxRuntime`, the option types it
//! consumes, and the `ready_sbx_agent` entry point the Layer 1 ready flow
//! drives. The first driver, `DSbxBackend` (Docker Sandboxes), is implemented
//! in WI 0090: kit emission, lifecycle, credential injection, session config.

mod backend;
mod dsbx;
pub mod naming;
pub mod options;
pub mod runtime;

pub use naming::{generate_sandbox_name, sandbox_name_for};
pub use options::{ResolvedSandboxOptions, SandboxOption};
pub use runtime::SandboxRuntime;

/// Prepare the Docker Sandbox runtime for a single agent at `awman ready`
/// time: check the `sbx` binary and login, emit the agent's kit, register
/// credentials, and validate the kit. Drives the sbx-specific ready phases on
/// behalf of the Layer 1 ready engine, reporting every `sbx` subprocess on
/// `sink`.
///
/// `no_cache` removes the agent's existing awman sandboxes (`sbx rm`) before
/// re-emitting the kit, so the next launch re-runs the kit install from a
/// clean state.
pub fn ready_sbx_agent(
    agent: &str,
    no_cache: bool,
    sink: &mut dyn crate::data::message::UserMessageSink,
) -> Result<(), crate::engine::error::EngineError> {
    dsbx::ready_agent(agent, no_cache, sink)
}
