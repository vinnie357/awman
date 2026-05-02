//! `HeadlessStartCommandFrontend` impl for the CLI.
//!
//! Per WI 0069 §3, `HeadlessStartCommand` (Layer 2) is parameterised by a
//! `HeadlessStartCommandFrontend` trait that exposes `serve_until_shutdown`.
//! The CLI frontend's impl is the one place in the codebase that may call
//! `crate::frontend::headless::serve(...)` — Layer 3 → Layer 3 is a peer
//! call, while Layer 2 → Layer 3 would be an upward call and is forbidden.
//!
//! WI 0069 leaves the actual server unimplemented (the headless frontend is
//! WI 0071). Until that lands, the CLI's `serve_until_shutdown` returns a
//! `CommandError::HeadlessUnavailable` so the user sees a clear error.

use crate::command::commands::headless::HeadlessStartCommandFrontend;
use crate::command::error::CommandError;

use crate::frontend::cli::command_frontend::CliFrontend;

impl HeadlessStartCommandFrontend for CliFrontend {
    fn serve_until_shutdown(&mut self) -> Result<(), CommandError> {
        // The headless server itself is implemented by WI 0071; until then
        // we surface a typed error rather than silently succeeding.
        Err(CommandError::NotImplemented(
            "headless server lands in work item 0071",
        ))
    }
}
