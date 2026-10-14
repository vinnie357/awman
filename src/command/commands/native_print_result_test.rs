//! Frozen real command completion contract; production must use this seam.
use crate::command::error::CommandError;
use crate::data::session::AgentName;
use crate::engine::agent_runtime::AgentExitInfo;
use crate::engine::error::EngineError;
use std::error::Error;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

#[test]
fn native_execution_wait_error_preserves_the_engine_error_and_source_cause() -> Result<()> {
    let agent = AgentName::new("agy")?;
    let outcome = super::finish_exec_prompt(
        &agent,
        true,
        Err(EngineError::Container("native wait boundary fault".into())),
    );
    assert!(
        matches!(outcome, Err(CommandError::Engine(EngineError::Container(ref cause))) if cause == "native wait boundary fault"),
        "native completion discarded or replaced its execution error: {outcome:?}"
    );
    Ok(())
}

#[test]
fn native_completion_retains_provider_nonzero_exit_and_output_mode() -> Result<()> {
    let agent = AgentName::new("codex")?;
    let now = chrono::Utc::now();
    let outcome = super::finish_exec_prompt(
        &agent,
        true,
        Ok(AgentExitInfo {
            exit_code: 7,
            signal: None,
            started_at: now,
            ended_at: now,
        }),
    )?;
    assert_eq!(outcome.agent.as_deref(), Some("codex"));
    assert_eq!(outcome.exit_code, Some(7));
    assert!(outcome.native_print);
    Ok(())
}

#[test]
fn ordinary_completion_keeps_legacy_unknown_exit_outcome() -> Result<()> {
    let agent = AgentName::new("agy")?;
    let outcome = super::finish_exec_prompt(
        &agent,
        false,
        Err(EngineError::Container("ordinary wait fault".into())),
    )?;
    assert_eq!(outcome.agent.as_deref(), Some("agy"));
    assert_eq!(outcome.exit_code, None);
    assert!(!outcome.native_print);
    Ok(())
}
