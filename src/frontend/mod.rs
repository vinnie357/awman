//! Layer 3 — frontends.
//!
//! Three independent implementations consume `Dispatch` (Layer 2),
//! `SessionManager` (Layer 0), and the per-command frontend traits
//! (Layers 1 + 2):
//!
//! - [`cli`]    — argv-driven, stdout/stderr/stdin rendering.
//! - [`tui`]    — Ratatui-based interactive terminal UI (placeholder; see
//!   work item 0070).
//! - [`headless`] — HTTP server (placeholder; see work item 0071).
//!
//! Frontends contain NO business logic; every behavioral decision lives in
//! Layer 2.

pub mod cli;
pub mod headless;
pub mod tui;
