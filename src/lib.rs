//! `amux` library — Layer 0–3 of the grand architecture refactor.
//!
//! `src/main.rs` (Layer 4) consumes this library to build the user-facing
//! `amux` binary. The four layers exposed below are wired together as
//! described in `aspec/architecture/2026-grand-architecture.md`:
//!
//! - [`data`] (Layer 0) — config, filesystem, session, workflow state.
//! - [`engine`] (Layer 1) — container/git/overlay/auth/agent/workflow engines.
//! - [`command`] (Layer 2) — `*Command` types, `Dispatch`, `CommandCatalogue`.
//! - [`frontend`] (Layer 3) — CLI / TUI / headless presentations of Layer 2.

#![forbid(unsafe_code)]
// Layer 1 / 2 / 3 carry types that are still being exercised across the
// refactor; suppress dead-code warnings here so partial wiring does not
// fail CI. Per WI 0072 this attribute is removed once oldsrc/ is deleted.
#![allow(dead_code)]

pub mod command;
pub mod data;
pub mod engine;
pub mod frontend;
