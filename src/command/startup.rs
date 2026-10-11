//! Layer 2 startup orchestration for the interactive binary hosts.
//!
//! `Startup` owns the ordered transition from process facts to the session and
//! engine bundle a frontend consumes. Keeping that transition here prevents a
//! new frontend from accidentally opening `RepoConfig` before its legacy path
//! has been migrated.

use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::command::dispatch::catalogue::CommandCatalogue;
use crate::command::dispatch::Engines;
use crate::data::config::env::EnvSnapshot;
use crate::data::config::global::GlobalConfig;
use crate::data::error::DataError;
use crate::data::migration;
use crate::data::session::{GitRootResolver, Session, SessionOpenOptions};
use crate::engine::error::EngineError;
use crate::engine::git::GitEngine;

/// The command-aware startup coordinator for one binary invocation.
pub struct Startup {
    command_path: Vec<String>,
}

impl Startup {
    /// Create startup orchestration for the parsed command path.
    pub fn new(command_path: Vec<String>) -> Self {
        Self { command_path }
    }

    /// Return the catalogue-owned hint for a removed flag before clap parses.
    pub fn removed_flag_hint<I>(&self, args: I) -> Option<String>
    where
        I: IntoIterator<Item = String>,
    {
        CommandCatalogue::get().removed_flag_hint(args)
    }

    /// Perform ordered application startup for `working_dir` and `env`.
    pub fn run(&self, working_dir: PathBuf, env: EnvSnapshot) -> Result<StartupOutcome> {
        let mut messages = Vec::new();

        // One-time migration from legacy amux paths and env vars.
        if let Some(msg) = migration::migrate_global_dir() {
            messages.push(msg);
        }
        messages.extend(migration::check_deprecated_env_vars());

        let global_config = GlobalConfig::load_with(&env).unwrap_or_default();
        let path_refs: Vec<&str> = self.command_path.iter().map(String::as_str).collect();
        // Runtime detection + the CLI/TUI fallback policy live on the Layer 2
        // `Engines` type. `fatal_runtime_error` is `Some` only when the configured
        // `runtime:` is invalid and the TUI is about to start: the TUI boots just
        // far enough to present a fatal modal with this message and quits on Enter.
        let (detected, fatal_runtime_error) =
            Engines::detect(CommandCatalogue::get(), &global_config, &path_refs).map_err(
                |error| match error {
                    EngineError::UnknownRuntime { .. } => anyhow::Error::new(error),
                    other => anyhow::Error::new(other).context("failed to detect agent runtime"),
                },
            )?;
        let git_engine = GitEngine::new();

        // Resolve git root first so we can migrate the repo-local `.amux/` → `.awman/`
        // BEFORE `Session::open` reads `RepoConfig` from disk. If we deferred this,
        // a user's first post-rename run would silently fall back to default repo
        // config because the load would miss the legacy `.amux/config.json`.
        let git_root = match git_engine.resolve(&working_dir) {
            Ok(root) => root,
            Err(DataError::GitRootNotFound { .. }) => working_dir.clone(),
            Err(other) => {
                return Err(anyhow::Error::new(other).context("failed to resolve git root"));
            }
        };
        if let Some(msg) = migration::migrate_repo_dir(&git_root) {
            messages.push(msg);
        }

        let session = Session::open_at_git_root(
            working_dir,
            git_root,
            SessionOpenOptions {
                env: Some(env),
                ..Default::default()
            },
        )
        .context("failed to open session")?;
        // Retain the exact handle selected by `Engines::detect`. Detection has
        // already applied the catalogue fallback and command-specific runtime
        // policy, so rebuilding here could select a different runtime.
        let engines =
            Engines::from_detected(detected, &session).context("failed to construct engines")?;

        Ok(StartupOutcome::new(
            session,
            engines,
            fatal_runtime_error,
            messages,
        ))
    }
}

/// The session, engines, and presentation data produced by [`Startup`].
pub struct StartupOutcome {
    pub session: Session,
    pub engines: Engines,
    pub fatal_runtime_error: Option<String>,
    messages: Vec<String>,
}

impl StartupOutcome {
    fn new(
        session: Session,
        engines: Engines,
        fatal_runtime_error: Option<String>,
        messages: Vec<String>,
    ) -> Self {
        Self {
            session,
            engines,
            fatal_runtime_error,
            messages,
        }
    }

    /// Messages collected during startup in the order they must be presented.
    pub fn messages(&self) -> &[String] {
        &self.messages
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::config::env::AWMAN_CONFIG_HOME;

    const HOME_FIXTURE_CHILD: &str = "AWMAN_STARTUP_HOME_FIXTURE_CHILD";
    const HOME_FIXTURE_ENTRY: &str = "AWMAN_STARTUP_HOME_FIXTURE_ENTRY";
    const HOME_FIXTURE_COMPLETION: &str = "AWMAN_STARTUP_HOME_FIXTURE_COMPLETION";

    fn enter_isolated_home_child(exact_test: &str) -> bool {
        if std::env::var(HOME_FIXTURE_CHILD).ok().as_deref() == Some(exact_test) {
            let entry =
                std::env::var_os(HOME_FIXTURE_ENTRY).expect("isolated Startup fixture entry path");
            std::fs::write(entry, exact_test).expect("record isolated Startup fixture entry");
            return true;
        }

        let proof = tempfile::tempdir().expect("isolated Startup proof directory");
        let home = proof.path().join("home");
        std::fs::create_dir(&home).expect("isolated Startup HOME");
        let entry = proof.path().join("entered");
        let completion = proof.path().join("completed");
        let mut child = std::process::Command::new(
            std::env::current_exe().expect("current Startup unit-test binary"),
        );
        child
            .args(["--exact", exact_test, "--nocapture"])
            .env(HOME_FIXTURE_CHILD, exact_test)
            .env(HOME_FIXTURE_ENTRY, &entry)
            .env(HOME_FIXTURE_COMPLETION, &completion)
            .env("HOME", &home)
            .env("USERPROFILE", &home);
        for legacy in [
            "AMUX_CONFIG_HOME",
            "AMUX_API_ROOT",
            "AMUX_OVERLAYS",
            "AMUX_REMOTE_ADDR",
            "AMUX_REMOTE_SESSION",
            "AMUX_API_KEY",
        ] {
            child.env_remove(legacy);
        }
        let mut child = child.spawn().expect("spawn isolated Startup fixture");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            match child.try_wait().expect("poll isolated Startup fixture") {
                Some(status) => {
                    assert!(
                        status.success(),
                        "isolated Startup fixture failed: {exact_test}"
                    );
                    assert_eq!(
                        std::fs::read_to_string(&entry)
                            .expect("isolated Startup fixture entry proof"),
                        exact_test,
                    );
                    assert_eq!(
                        std::fs::read_to_string(&completion)
                            .expect("isolated Startup fixture completion proof"),
                        exact_test,
                    );
                    return false;
                }
                None if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                None => {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("isolated Startup fixture timed out: {exact_test}");
                }
            }
        }
    }

    fn complete_isolated_home_child(exact_test: &str) {
        assert_eq!(
            std::env::var(HOME_FIXTURE_CHILD).ok().as_deref(),
            Some(exact_test),
        );
        let completion = std::env::var_os(HOME_FIXTURE_COMPLETION)
            .expect("isolated Startup fixture completion path");
        std::fs::write(completion, exact_test).expect("record isolated Startup fixture completion");
    }

    struct StartupFixture {
        root: tempfile::TempDir,
        env: EnvSnapshot,
    }

    impl StartupFixture {
        fn with_runtime(runtime: Option<&str>) -> Self {
            let root = tempfile::tempdir().expect("temporary startup fixture");
            let config_home = root.path().join("config-home");
            let working_dir = root.path().join("workspace");
            std::fs::create_dir_all(&working_dir).expect("create isolated working directory");
            let env = EnvSnapshot::with_overrides([(
                AWMAN_CONFIG_HOME,
                config_home.to_str().expect("UTF-8 temporary path"),
            )]);
            if let Some(runtime) = runtime {
                GlobalConfig {
                    runtime: Some(runtime.to_owned()),
                    ..Default::default()
                }
                .save_with(&env)
                .expect("write isolated global config");
            }
            Self { root, env }
        }

        fn working_dir(&self) -> PathBuf {
            self.root.path().join("workspace")
        }
    }

    #[test]
    fn startup_default_config_command_uses_detected_default_runtime() {
        const EXACT: &str =
            "command::startup::tests::startup_default_config_command_uses_detected_default_runtime";
        if !enter_isolated_home_child(EXACT) {
            return;
        }
        let fixture = StartupFixture::with_runtime(None);
        let outcome = Startup::new(vec!["config".into(), "show".into()])
            .run(fixture.working_dir(), fixture.env.clone())
            .expect("ordinary config startup with defaults");

        assert_eq!(outcome.engines.runtime.runtime_name(), "docker");
        assert!(outcome.fatal_runtime_error.is_none());
        complete_isolated_home_child(EXACT);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn startup_config_command_reuses_detected_fallback_for_unavailable_apple_runtime() {
        const EXACT: &str = "command::startup::tests::startup_config_command_reuses_detected_fallback_for_unavailable_apple_runtime";
        if !enter_isolated_home_child(EXACT) {
            return;
        }
        let fixture = StartupFixture::with_runtime(Some("apple-containers"));
        let outcome = Startup::new(vec!["config".into(), "show".into()])
            .run(fixture.working_dir(), fixture.env.clone())
            .expect("config must remain available through the detected fallback");

        assert_eq!(
            outcome.engines.runtime.runtime_name(),
            "docker",
            "Startup must assemble engines from Engines::detect's fallback handle"
        );
        assert!(outcome.fatal_runtime_error.is_none());
        complete_isolated_home_child(EXACT);
    }

    #[test]
    fn startup_unknown_runtime_cli_remains_fatal() {
        const EXACT: &str = "command::startup::tests::startup_unknown_runtime_cli_remains_fatal";
        if !enter_isolated_home_child(EXACT) {
            return;
        }
        let fixture = StartupFixture::with_runtime(Some("not-a-runtime"));
        let error = match Startup::new(vec!["config".into(), "show".into()])
            .run(fixture.working_dir(), fixture.env.clone())
        {
            Err(error) => error,
            Ok(_) => panic!("unknown CLI runtime must be rejected"),
        };

        assert!(matches!(
            error.downcast_ref::<EngineError>(),
            Some(EngineError::UnknownRuntime { value, .. }) if value == "not-a-runtime"
        ));
        complete_isolated_home_child(EXACT);
    }

    #[test]
    fn startup_unknown_runtime_bare_tui_preserves_fatal_modal_and_inert_runtime() {
        const EXACT: &str = "command::startup::tests::startup_unknown_runtime_bare_tui_preserves_fatal_modal_and_inert_runtime";
        if !enter_isolated_home_child(EXACT) {
            return;
        }
        let fixture = StartupFixture::with_runtime(Some("not-a-runtime"));
        let outcome = Startup::new(Vec::new())
            .run(fixture.working_dir(), fixture.env.clone())
            .expect("bare TUI must start only far enough to show the fatal modal");

        assert_eq!(outcome.engines.runtime.runtime_name(), "docker");
        let fatal = outcome
            .fatal_runtime_error
            .expect("bare TUI must retain a fatal runtime message");
        assert!(fatal.contains("not-a-runtime"), "fatal message: {fatal}");
        complete_isolated_home_child(EXACT);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn startup_runtime_required_command_rejects_unavailable_apple_runtime() {
        const EXACT: &str = "command::startup::tests::startup_runtime_required_command_rejects_unavailable_apple_runtime";
        if !enter_isolated_home_child(EXACT) {
            return;
        }
        let fixture = StartupFixture::with_runtime(Some("apple-containers"));
        let error = match Startup::new(vec!["status".into()])
            .run(fixture.working_dir(), fixture.env.clone())
        {
            Err(error) => error,
            Ok(_) => panic!("runtime-required command must reject unavailable Apple Containers"),
        };

        assert!(matches!(
            error.downcast_ref::<EngineError>(),
            Some(EngineError::BackendUnsupportedOnPlatform { backend, platform })
                if backend == "apple-containers" && platform == "linux"
        ));
        complete_isolated_home_child(EXACT);
    }
}
