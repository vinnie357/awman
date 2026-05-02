//! TUI frontend — placeholder.
//!
//! The full TUI implementation (~21k lines ported and adapted from
//! `oldsrc/tui/`) is the deliverable of work item
//! `0070-grand-architecture-tui-frontend.md`. Until then, bare invocations
//! of `amux` print a one-line notice and exit cleanly so the CLI surface
//! introduced in `0069-…` is usable end-to-end.

use std::process::ExitCode;

use crate::frontend::cli::RuntimeContext;

/// Entry point invoked by `main.rs` for bare (no-subcommand) launches.
///
/// **Placeholder implementation** — work item 0070 replaces the body with
/// the real Ratatui event loop. The signature is the public contract that
/// 0070 must preserve.
pub async fn run(_matches: clap::ArgMatches, _ctx: RuntimeContext) -> ExitCode {
    eprintln!(
        "amux: TUI is not yet wired up in this build. \
         Run with a subcommand (try `amux --help`) for the CLI flow. \
         The TUI ships in work item 0070."
    );
    ExitCode::from(0)
}

#[cfg(test)]
mod tests {
    use crate::command::dispatch::catalogue::CommandCatalogue;

    /// The TUI is selected when no subcommand is given. Verify that the clap
    /// layer agrees: a bare `amux` invocation has no subcommand name.
    #[test]
    fn bare_invocation_has_no_subcommand() {
        let cmd = CommandCatalogue::get().build_clap_command();
        let m = cmd.try_get_matches_from(["amux"]).unwrap();
        assert!(
            m.subcommand_name().is_none(),
            "bare `amux` must have no subcommand — main.rs uses this to route to TUI"
        );
    }

    /// Any subcommand routes to CLI, NOT TUI. Verify a representative sample.
    #[test]
    fn subcommand_presence_routes_away_from_tui() {
        let cmd = CommandCatalogue::get().build_clap_command();
        for argv in [
            vec!["amux", "status"],
            vec!["amux", "ready"],
            vec!["amux", "chat"],
        ] {
            let m = cmd.clone().try_get_matches_from(&argv).unwrap();
            assert!(
                m.subcommand_name().is_some(),
                "{argv:?} must have a subcommand name"
            );
        }
    }
}
