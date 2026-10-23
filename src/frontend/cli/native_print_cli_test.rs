//! Frozen public catalogue, command preflight, and output contract.
use super::{command_path_from_matches, CliFrontend};
use crate::command::commands::exec_prompt::ExecPromptOutcome;
use crate::command::{BuiltCommand, CommandCatalogue, CommandOutcome, Dispatch, Engines};
use crate::data::config::env::EnvSnapshot;
use crate::data::session::{Session, SessionOpenOptions, StaticGitRootResolver};
use std::error::Error;
use std::sync::Arc;
use tokio::sync::RwLock;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

fn parse(parts: &[&str]) -> Result<clap::ArgMatches> {
    Ok(CommandCatalogue::get()
        .build_clap_command()
        .try_get_matches_from(parts)?)
}

fn build(parts: &[&str]) -> Result<BuiltCommand> {
    let root = tempfile::tempdir()?;
    let resolver = StaticGitRootResolver::new(root.path());
    let session = Session::open(
        root.path().to_path_buf(),
        &resolver,
        SessionOpenOptions {
            env: Some(EnvSnapshot::with_overrides([(
                "AWMAN_CONFIG_HOME".to_string(),
                root.path().to_string_lossy().into_owned(),
            )])),
            ..Default::default()
        },
    )?;
    let matches = parse(parts)?;
    let dispatch = Dispatch::new(
        CliFrontend::new(matches),
        Arc::new(RwLock::new(session)),
        Engines::for_tests(root.path()),
    );
    Ok(dispatch.build_command(&["exec", "prompt"])?)
}

#[test]
fn native_cli_is_scoped_to_exec_prompt_and_forces_explicit_noninteractive_on_tty() -> Result<()> {
    for agent in ["claude", "codex", "agy"] {
        let matches = parse(&[
            "awman",
            "exec",
            "prompt",
            "--native-print",
            "--agent",
            agent,
            "--model",
            "native-model",
            "review input",
        ])?;
        let path = command_path_from_matches(&matches);
        assert_eq!(path, ["exec", "prompt"]);
        assert!(CliFrontend::explicit_non_interactive(&matches, &path));
    }
    assert!(parse(&["awman", "chat", "--native-print"]).is_err());
    assert!(parse(&[
        "awman",
        "exec",
        "workflow",
        "--native-print",
        "workflow.toml"
    ])
    .is_err());
    Ok(())
}

#[test]
fn native_command_preflight_requires_explicit_canonical_provider_and_model() -> Result<()> {
    for invalid in [
        vec![
            "awman",
            "exec",
            "prompt",
            "--native-print",
            "--model",
            "native-model",
            "review input",
        ],
        vec![
            "awman",
            "exec",
            "prompt",
            "--native-print",
            "--agent",
            "agy",
            "review input",
        ],
        vec![
            "awman",
            "exec",
            "prompt",
            "--native-print",
            "--agent",
            "opencode",
            "--model",
            "native-model",
            "review input",
        ],
        vec![
            "awman",
            "exec",
            "prompt",
            "--native-print",
            "--agent",
            "codex",
            "--model",
            " ",
            "review input",
        ],
        vec![
            "awman",
            "exec",
            "prompt",
            "--native-print",
            "--agent",
            "agy",
            "--model",
            "native-model",
            "--native-session",
            " ",
            "review input",
        ],
        vec![
            "awman",
            "exec",
            "prompt",
            "--native-session",
            "session-id",
            "review input",
        ],
    ] {
        assert!(
            build(&invalid).is_err(),
            "invalid native request must fail during command preflight: {invalid:?}"
        );
    }
    for agent in ["claude", "codex", "agy"] {
        assert!(matches!(
            build(&[
                "awman",
                "exec",
                "prompt",
                "--native-print",
                "--agent",
                agent,
                "--model",
                "native-model",
                "--native-session",
                "session-id",
                "review input"
            ])?,
            BuiltCommand::ExecPrompt(_)
        ));
    }
    Ok(())
}

#[test]
fn native_nonzero_outcome_never_adds_a_stdout_summary_and_keeps_exit_code() {
    for code in [0, 7, 130] {
        let outcome = CommandOutcome::ExecPrompt(ExecPromptOutcome {
            agent: Some("agy".into()),
            exit_code: Some(code),
            native_print: true,
        });
        assert_eq!(super::super::format_outcome(&outcome, false), None);
        assert_eq!(outcome.exit_code(), code);
        assert_eq!(super::super::outcome_exit_code(&outcome), code as u8);
    }
}

#[test]
fn ordinary_exec_outcome_and_tty_selector_keep_existing_behavior() -> Result<()> {
    let matches = parse(&["awman", "exec", "prompt", "review input"])?;
    assert!(!CliFrontend::explicit_non_interactive(
        &matches,
        &command_path_from_matches(&matches)
    ));
    let outcome = CommandOutcome::ExecPrompt(ExecPromptOutcome {
        agent: Some("agy".into()),
        exit_code: Some(7),
        native_print: false,
    });
    assert_eq!(
        super::super::format_outcome(&outcome, false),
        Some("exec prompt ended with exit code 7.".into())
    );
    Ok(())
}
