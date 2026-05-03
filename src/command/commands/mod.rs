//! `src/command/commands/` — one struct per amux command.
//!
//! Each module contains the `*Command` struct (owning every flag value and
//! engine reference it needs), its `*CommandFrontend` trait (defining the
//! exact user-input methods that command requires), and the
//! `Command` impl whose `run_with_frontend(frontend) -> *Outcome` body holds
//! all of the command's business logic.

pub mod agent_auth;
pub mod agent_setup;
pub mod auth;
pub mod chat;
pub mod claws;
pub mod command_trait;
pub mod config;
pub mod download;
pub mod exec_prompt;
pub mod exec_workflow;
pub mod headless;
pub mod implement;
pub mod implement_prompts;
pub mod init;
pub mod mount_scope;
pub mod new;
pub mod ready;
pub mod remote;
pub(super) mod remote_client;
pub mod specs;
pub mod status;
pub mod status_tips;
pub mod worktree_lifecycle;

pub use command_trait::Command;

/// Parse a user-supplied overlay spec string in the form
/// `host:container` or `host:container:perm` (where perm is `ro` or `rw`).
///
/// Returns the parsed `DirectorySpec` or a descriptive error string on failure.
pub fn parse_overlay_spec(
    spec: &str,
) -> Result<crate::engine::overlay::DirectorySpec, String> {
    use crate::engine::container::options::OverlayPermission;
    use crate::engine::overlay::DirectorySpec;

    let parts: Vec<&str> = spec.splitn(3, ':').collect();
    if parts.len() < 2 {
        return Err(format!(
            "expected 'host:container' or 'host:container:perm', got '{spec}'"
        ));
    }
    let host = parts[0].to_string();
    if host.is_empty() {
        return Err("host path must not be empty".to_string());
    }
    let container = parts[1].to_string();
    if container.is_empty() {
        return Err("container path must not be empty".to_string());
    }
    if !container.starts_with('/') {
        return Err(format!("container path '{container}' must be absolute"));
    }
    let permission = match parts.get(2).copied() {
        None | Some("rw") | Some("") => OverlayPermission::ReadWrite,
        Some("ro") => OverlayPermission::ReadOnly,
        Some(other) => {
            return Err(format!(
                "unknown permission '{other}'; expected 'ro' or 'rw'"
            ));
        }
    };
    Ok(DirectorySpec {
        host,
        container,
        permission,
    })
}

#[cfg(test)]
mod overlay_spec_tests {
    use super::*;
    use crate::engine::container::options::OverlayPermission;

    #[test]
    fn parse_overlay_spec_host_container_default_rw() {
        let spec = parse_overlay_spec("/host/path:/container/path").unwrap();
        assert_eq!(spec.host, "/host/path");
        assert_eq!(spec.container, "/container/path");
        assert_eq!(spec.permission, OverlayPermission::ReadWrite);
    }

    #[test]
    fn parse_overlay_spec_with_ro_permission() {
        let spec = parse_overlay_spec("/host/path:/container/path:ro").unwrap();
        assert_eq!(spec.permission, OverlayPermission::ReadOnly);
    }

    #[test]
    fn parse_overlay_spec_with_rw_permission() {
        let spec = parse_overlay_spec("/host/path:/container/path:rw").unwrap();
        assert_eq!(spec.permission, OverlayPermission::ReadWrite);
    }

    #[test]
    fn parse_overlay_spec_missing_container_returns_error() {
        let result = parse_overlay_spec("/host/only");
        assert!(result.is_err(), "must error when container path is missing");
    }

    #[test]
    fn parse_overlay_spec_relative_container_path_returns_error() {
        let result = parse_overlay_spec("/host/path:relative/path");
        assert!(result.is_err(), "must error for relative container path");
    }

    #[test]
    fn parse_overlay_spec_unknown_permission_returns_error() {
        let result = parse_overlay_spec("/host:/container:rx");
        assert!(result.is_err(), "must error for unknown permission 'rx'");
    }

    #[test]
    fn parse_overlay_spec_empty_host_returns_error() {
        let result = parse_overlay_spec(":/container/path");
        assert!(result.is_err(), "must error for empty host path");
    }
}
