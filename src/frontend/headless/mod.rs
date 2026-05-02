//! Headless HTTP frontend — placeholder.
//!
//! The full headless server (port of `oldsrc/commands/headless/server.rs`
//! re-plumbed to dispatch through `Dispatch::run_command` instead of
//! spawning a child `amux` process) is the deliverable of work item
//! `0071-grand-architecture-headless-frontend.md`. Until then, the CLI's
//! `HeadlessStartCommandFrontend` impl returns a "not yet implemented"
//! error instead of starting the server.

use crate::command::error::CommandError;

/// Configuration handed in by the `headless start` command path.
///
/// Populated by Layer 2 from `HeadlessStartFlags` plus `GlobalConfig` and
/// the resolved workdir allowlist. Layer 3 consumes this verbatim.
#[derive(Debug, Clone)]
pub struct HeadlessServeConfig {
    pub port: u16,
    pub workdirs: Vec<std::path::PathBuf>,
    pub dangerously_skip_auth: bool,
}

/// Entry point that the CLI's `HeadlessStartCommandFrontend` impl will
/// call once the headless frontend ships in work item 0071.
///
/// **Placeholder implementation** — returns `CommandError::NotImplemented`.
pub async fn serve(_config: HeadlessServeConfig) -> Result<(), CommandError> {
    Err(CommandError::NotImplemented(
        "headless server lands in work item 0071",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn serve_returns_not_implemented_error() {
        let config = HeadlessServeConfig {
            port: 9876,
            workdirs: vec![],
            dangerously_skip_auth: false,
        };
        let result = serve(config).await;
        assert!(
            result.is_err(),
            "serve placeholder must return an error until WI 0071 lands"
        );
        assert!(
            matches!(result.unwrap_err(), CommandError::NotImplemented(_)),
            "serve placeholder must return CommandError::NotImplemented"
        );
    }

    #[tokio::test]
    async fn serve_config_fields_are_structurally_valid() {
        // Ensure HeadlessServeConfig can be constructed with all fields.
        let config = HeadlessServeConfig {
            port: 1234,
            workdirs: vec![
                std::path::PathBuf::from("/tmp/workdir1"),
                std::path::PathBuf::from("/tmp/workdir2"),
            ],
            dangerously_skip_auth: true,
        };
        assert_eq!(config.port, 1234);
        assert_eq!(config.workdirs.len(), 2);
        assert!(config.dangerously_skip_auth);
        // The serve call returns NotImplemented — this test just validates
        // the config shape is correct.
        let _ = serve(config).await;
    }
}
