#![forbid(unsafe_code)]
//! Layer 4 — the `awman` binary entrypoint.
//!
//! Per `aspec/architecture/2026-grand-architecture.md`, `main.rs`
//! contains no business logic: it builds clap from `CommandCatalogue`,
//! parses argv, constructs the engines + session, and dispatches to either
//! the CLI frontend (when a subcommand is present) or the TUI frontend
//! (bare invocation).

use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result};

use awman::command::dispatch::catalogue::CommandCatalogue;
use awman::command::dispatch::Engines;
use awman::data::config::global::GlobalConfig;
use awman::data::error::DataError;
use awman::data::migration;
use awman::data::config::env::Env;
use awman::data::session::{GitRootResolver, Session, SessionOpenOptions};
use awman::engine::agent::AgentEngine;
use awman::engine::auth::AuthEngine;
use awman::engine::container::ContainerRuntime;
use awman::engine::git::GitEngine;
use awman::engine::overlay::OverlayEngine;
use awman::frontend::cli::{self, RuntimeContext};
use awman::frontend::tui;

#[tokio::main]
async fn main() -> Result<ExitCode> {
    let clap_cmd = CommandCatalogue::get().build_clap_command();
    let matches = clap_cmd.get_matches();

    init_tracing();

    // One-time migration from legacy amux paths and env vars.
    if let Some(msg) = migration::migrate_global_dir() {
        eprintln!("{msg}");
    }
    for warning in migration::check_deprecated_env_vars() {
        eprintln!("{warning}");
    }

    let global_config = GlobalConfig::load().unwrap_or_default();
    let runtime = Arc::new(
        ContainerRuntime::detect(&global_config).context("failed to detect container runtime")?,
    );
    let git_engine = Arc::new(GitEngine::new());

    let working_dir = std::env::current_dir().context("could not read current directory")?;

    // Resolve git root first so we can migrate the repo-local `.amux/` → `.awman/`
    // BEFORE `Session::open` reads `RepoConfig` from disk. If we deferred this,
    // a user's first post-rename run would silently fall back to default repo
    // config because the load would miss the legacy `.amux/config.json`.
    let git_root = match git_engine.resolve(&working_dir) {
        Ok(root) => root,
        Err(DataError::GitRootNotFound { .. }) => working_dir.clone(),
        Err(other) => return Err(anyhow::Error::new(other).context("failed to resolve git root")),
    };
    if let Some(msg) = migration::migrate_repo_dir(&git_root) {
        eprintln!("{msg}");
    }

    let session = Session::open_at_git_root(
        working_dir.clone(),
        git_root,
        SessionOpenOptions {
            env: Some(Env::from_process()),
            ..Default::default()
        },
    )
    .context("failed to open session")?;

    let overlay_engine =
        Arc::new(OverlayEngine::new(&session).context("failed to construct overlay engine")?);
    let auth_engine =
        Arc::new(AuthEngine::new(&session).context("failed to construct auth engine")?);
    let agent_engine = Arc::new(AgentEngine::new(overlay_engine.clone(), runtime.clone()));
    let workflow_state_store = Arc::new(awman::data::EngineWorkflowStateStore::at_git_root(
        session.git_root().to_path_buf(),
    ));

    let engines = Engines {
        runtime,
        git_engine,
        overlay_engine,
        auth_engine,
        agent_engine,
        workflow_state_store,
    };

    let ctx = RuntimeContext::new(session, engines);

    if matches.subcommand_name().is_some() {
        Ok(cli::run(matches, ctx).await)
    } else {
        Ok(tui::run(matches, ctx).await)
    }
}

/// Initialize the global tracing subscriber once at process start.
///
/// Without this, every `tracing::info!`/`warn!`/`error!` call in the
/// codebase (notably the API-server startup messages in `frontend::api`)
/// is silently dropped, which made `awman api start` look like it was
/// hanging until Ctrl-C. We write to stderr (so stdout stays clean for
/// `--json` callers), default to `info`-level for awman code, and honor
/// `RUST_LOG` for overrides. ANSI colors are auto-enabled by the `fmt`
/// layer when stderr is a TTY.
///
/// The TUI initializes its own renderer on a separate path; tracing
/// records still go to stderr but are not visible behind the alt-screen
/// until the TUI exits, which is the same trade-off any tracing-enabled
/// TUI accepts.
fn init_tracing() {
    use tracing_subscriber::fmt::{format::Writer, time::FormatTime};
    use tracing_subscriber::{fmt, EnvFilter};

    /// Compact wall-clock timer: `HH:MM:SS.mmm`. The default `SystemTime`
    /// timer prints a full ISO-8601 string per line which dominates short
    /// API log records like `info: Restored session`.
    struct ShortLocalTime;
    impl FormatTime for ShortLocalTime {
        fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
            write!(w, "{}", chrono::Local::now().format("%H:%M:%S%.3f"))
        }
    }

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .with_timer(ShortLocalTime)
        .compact()
        .try_init();
}

// ─── Layer 4 routing tests ────────────────────────────────────────────────────
//
// `main` is too integrated to call in unit tests (it requires live engines and
// a real session). Instead we test the **routing logic** directly: the condition
// `matches.subcommand_name().is_some()` is what drives the cli-vs-tui branch.
// These tests exercise that predicate with synthetic `ArgMatches`.

#[cfg(test)]
mod tests {
    use awman::command::dispatch::catalogue::CommandCatalogue;
    use awman::frontend::cli::command_path_from_matches;

    /// A subcommand in argv → `subcommand_name().is_some()` → CLI branch.
    #[test]
    fn subcommand_present_signals_cli_branch() {
        let cmd = CommandCatalogue::get().build_clap_command();
        for argv in [
            vec!["awman", "status"],
            vec!["awman", "ready"],
            vec!["awman", "chat"],
            vec!["awman", "init"],
            vec!["awman", "exec", "workflow", "wf.toml"],
            vec!["awman", "api", "start"],
            vec!["awman", "remote", "session", "start"],
        ] {
            let m = cmd
                .clone()
                .try_get_matches_from(&argv)
                .unwrap_or_else(|e| panic!("failed to parse {argv:?}: {e}"));
            assert!(
                m.subcommand_name().is_some(),
                "{argv:?} must have a subcommand — routes to CLI"
            );
            // command_path_from_matches must also return a non-empty path.
            let path = command_path_from_matches(&m);
            assert!(!path.is_empty(), "{argv:?} must produce a non-empty path");
        }
    }

    /// Bare `awman` → `subcommand_name().is_none()` → TUI branch.
    #[test]
    fn bare_invocation_signals_tui_branch() {
        let cmd = CommandCatalogue::get().build_clap_command();
        let m = cmd.try_get_matches_from(["awman"]).unwrap();
        assert!(
            m.subcommand_name().is_none(),
            "bare `awman` must have no subcommand — routes to TUI"
        );
        let path = command_path_from_matches(&m);
        assert!(
            path.is_empty(),
            "bare invocation must produce an empty path"
        );
    }

    /// Aliases also route through the CLI branch correctly.
    #[test]
    fn exec_workflow_alias_wf_routes_to_cli() {
        let cmd = CommandCatalogue::get().build_clap_command();
        let m = cmd
            .try_get_matches_from(["awman", "exec", "wf", "wf.toml"])
            .unwrap();
        assert!(m.subcommand_name().is_some());
        let path = command_path_from_matches(&m);
        // Clap resolves the alias to canonical `workflow`.
        assert_eq!(path, vec!["exec", "workflow"]);
    }
}
