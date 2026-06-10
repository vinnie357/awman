//! Kit emission — renders a per-agent `spec.yaml` plus the bundled
//! `apply-session-config.sh` startup script into a kit directory.
//!
//! Called from the Layer 1 ready flow (`awman ready` for the sbx runtime).
//! Credential **values** are never written into kit files — only credential
//! mappings/config, which already live in the embedded templates.

use std::path::Path;

use crate::data::templates::{sbx_apply_script_for, sbx_kit_template_for};
use crate::engine::error::EngineError;

/// Placeholder substituted with awman's version when rendering kit templates.
const AWMAN_VERSION_PLACEHOLDER: &str = "{{AWMAN_VERSION}}";

/// Renders kit specs and startup scripts for sbx agents.
pub(super) struct DSbxKitEmitter {
    awman_version: String,
}

impl Default for DSbxKitEmitter {
    fn default() -> Self {
        Self::new()
    }
}

impl DSbxKitEmitter {
    pub(super) fn new() -> Self {
        Self {
            awman_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    /// Render the kit for `agent` into `dest` (the kit directory):
    /// - `<dest>/spec.yaml`
    /// - `<dest>/files/home/.awman/apply-session-config.sh` (mode 0755)
    pub(super) fn emit_for_agent(&self, agent: &str, dest: &Path) -> Result<(), EngineError> {
        let template = sbx_kit_template_for(agent).ok_or_else(|| {
            EngineError::Sandbox(format!(
                "no sbx kit template bundled for agent '{agent}'"
            ))
        })?;
        let script = sbx_apply_script_for(agent).ok_or_else(|| {
            EngineError::Sandbox(format!(
                "no sbx startup script bundled for agent '{agent}'"
            ))
        })?;

        // 1. Render and write spec.yaml.
        let rendered = template.replace(AWMAN_VERSION_PLACEHOLDER, &self.awman_version);
        std::fs::create_dir_all(dest).map_err(|e| EngineError::io(dest, e))?;
        let spec_path = dest.join("spec.yaml");
        std::fs::write(&spec_path, rendered).map_err(|e| EngineError::io(&spec_path, e))?;

        // 2. Write the per-launch startup script into files/home/.awman/.
        let script_dir = dest.join("files").join("home").join(".awman");
        std::fs::create_dir_all(&script_dir).map_err(|e| EngineError::io(&script_dir, e))?;
        let script_path = script_dir.join("apply-session-config.sh");
        std::fs::write(&script_path, script).map_err(|e| EngineError::io(&script_path, e))?;
        set_executable(&script_path)?;

        Ok(())
    }
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<(), EngineError> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)
        .map_err(|e| EngineError::io(path, e))?
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).map_err(|e| EngineError::io(path, e))
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<(), EngineError> {
    // No executable bit on Windows; the kit's `commands.startup` invokes the
    // script via `bash`, so the mode is irrelevant there.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_spec_and_script_with_version_substituted() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("claude");
        DSbxKitEmitter::new().emit_for_agent("claude", &dest).unwrap();

        let spec = std::fs::read_to_string(dest.join("spec.yaml")).unwrap();
        assert!(spec.contains("kind: mixin"));
        assert!(
            !spec.contains("{{AWMAN_VERSION}}"),
            "version placeholder must be substituted"
        );
        assert!(spec.contains(env!("CARGO_PKG_VERSION")));

        let script_path = dest
            .join("files")
            .join("home")
            .join(".awman")
            .join("apply-session-config.sh");
        assert!(script_path.exists(), "startup script must be written");
        let script = std::fs::read_to_string(&script_path).unwrap();
        assert!(script.starts_with("#!/usr/bin/env bash"));
    }

    #[test]
    fn unknown_agent_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let err = DSbxKitEmitter::new()
            .emit_for_agent("not-an-agent", &tmp.path().join("x"))
            .unwrap_err();
        assert!(matches!(err, EngineError::Sandbox(_)));
    }

    #[cfg(unix)]
    #[test]
    fn startup_script_is_executable() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("codex");
        DSbxKitEmitter::new().emit_for_agent("codex", &dest).unwrap();
        let script_path = dest
            .join("files")
            .join("home")
            .join(".awman")
            .join("apply-session-config.sh");
        let mode = std::fs::metadata(&script_path).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111, "script must be executable");
    }

    // ─── Golden-file shape tests: mixin agents ────────────────────────────
    //
    // Mixin kits extend a Docker built-in template and must have `kind: mixin`,
    // no `agent:` block, a `network:` section listing the agent's API domain(s),
    // and a `credentials:` section with the right service mapping.

    fn emit_spec(agent: &str) -> String {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join(agent);
        DSbxKitEmitter::new().emit_for_agent(agent, &dest).unwrap();
        std::fs::read_to_string(dest.join("spec.yaml")).unwrap()
    }

    #[test]
    fn claude_kit_is_mixin_with_anthropic_credential() {
        let spec = emit_spec("claude");
        assert!(spec.contains("kind: mixin"), "claude must be kind: mixin");
        assert!(!spec.contains("\nagent:"), "mixin must not have an agent: block");
        assert!(spec.contains("extends: claude-code"), "claude must extend claude-code");
        assert!(spec.contains("api.anthropic.com"), "must list anthropic domain");
        assert!(spec.contains("ANTHROPIC_API_KEY: anthropic"), "must map ANTHROPIC_API_KEY");
    }

    #[test]
    fn codex_kit_is_mixin_with_openai_credential() {
        let spec = emit_spec("codex");
        assert!(spec.contains("kind: mixin"), "codex must be kind: mixin");
        assert!(!spec.contains("\nagent:"), "mixin must not have an agent: block");
        assert!(spec.contains("extends: codex"), "codex must extend codex template");
        assert!(spec.contains("api.openai.com"), "must list openai domain");
        assert!(spec.contains("OPENAI_API_KEY: openai"), "must map OPENAI_API_KEY");
    }

    #[test]
    fn gemini_kit_is_mixin_with_google_credential() {
        let spec = emit_spec("gemini");
        assert!(spec.contains("kind: mixin"), "gemini must be kind: mixin");
        assert!(!spec.contains("\nagent:"), "mixin must not have an agent: block");
        assert!(spec.contains("extends: gemini"), "gemini must extend gemini template");
        assert!(spec.contains("generativelanguage.googleapis.com"), "must list google domain");
        assert!(spec.contains("GEMINI_API_KEY: google"), "must map GEMINI_API_KEY");
    }

    #[test]
    fn copilot_kit_is_mixin_with_github_credential() {
        let spec = emit_spec("copilot");
        assert!(spec.contains("kind: mixin"), "copilot must be kind: mixin");
        assert!(!spec.contains("\nagent:"), "mixin must not have an agent: block");
        assert!(spec.contains("extends: copilot"), "copilot must extend copilot template");
        assert!(spec.contains("api.githubcopilot.com"), "must list copilot domain");
        assert!(spec.contains("GITHUB_TOKEN: github"), "must map GITHUB_TOKEN");
    }

    #[test]
    fn opencode_kit_is_mixin_with_anthropic_credential() {
        let spec = emit_spec("opencode");
        assert!(spec.contains("kind: mixin"), "opencode must be kind: mixin");
        assert!(!spec.contains("\nagent:"), "mixin must not have an agent: block");
        assert!(spec.contains("extends: opencode"), "opencode must extend opencode template");
        assert!(spec.contains("api.anthropic.com"), "must list anthropic domain");
        assert!(spec.contains("ANTHROPIC_API_KEY: anthropic"), "must map ANTHROPIC_API_KEY");
    }

    // ─── Golden-file shape tests: agent kits ─────────────────────────────
    //
    // Agent kits install the agent binary themselves. They must have
    // `kind: agent`, an `agent:` block with the shell-docker base image, and a
    // `commands.install:` section with the npm installation script.

    #[test]
    fn antigravity_kit_is_agent_with_base_image_and_install() {
        let spec = emit_spec("antigravity");
        assert!(spec.contains("kind: agent"), "antigravity must be kind: agent");
        assert!(spec.contains("docker/sandbox-templates:shell-docker"), "must use shell-docker base image");
        assert!(spec.contains("install:"), "must have install commands");
        assert!(spec.contains("antigravity"), "install must reference the agent name");
        assert!(spec.contains("GEMINI_API_KEY: google"), "antigravity uses google credential");
    }

    #[test]
    fn crush_kit_is_agent_with_base_image_and_install() {
        let spec = emit_spec("crush");
        assert!(spec.contains("kind: agent"), "crush must be kind: agent");
        assert!(spec.contains("docker/sandbox-templates:shell-docker"), "must use shell-docker base image");
        assert!(spec.contains("install:"), "must have install commands");
        assert!(spec.contains("crush"), "install must reference the agent name");
    }

    #[test]
    fn maki_kit_is_agent_with_base_image_and_install() {
        let spec = emit_spec("maki");
        assert!(spec.contains("kind: agent"), "maki must be kind: agent");
        assert!(spec.contains("docker/sandbox-templates:shell-docker"), "must use shell-docker base image");
        assert!(spec.contains("install:"), "must have install commands");
    }

    #[test]
    fn cline_kit_is_agent_with_base_image_and_install() {
        let spec = emit_spec("cline");
        assert!(spec.contains("kind: agent"), "cline must be kind: agent");
        assert!(spec.contains("docker/sandbox-templates:shell-docker"), "must use shell-docker base image");
        assert!(spec.contains("install:"), "must have install commands");
    }

    // ─── Version substitution for all agents ─────────────────────────────

    #[test]
    fn all_agents_have_version_substituted() {
        let agents = [
            "claude", "codex", "gemini", "copilot", "opencode",
            "antigravity", "crush", "maki", "cline",
        ];
        for agent in &agents {
            let spec = emit_spec(agent);
            assert!(
                !spec.contains("{{AWMAN_VERSION}}"),
                "agent {agent}: version placeholder must be substituted"
            );
            assert!(
                spec.contains(env!("CARGO_PKG_VERSION")),
                "agent {agent}: current version must appear in spec.yaml"
            );
        }
    }

    // ─── All agents emit the startup script with executable mode ─────────

    #[cfg(unix)]
    #[test]
    fn all_agents_startup_script_is_executable() {
        use std::os::unix::fs::PermissionsExt;
        let agents = [
            "claude", "codex", "gemini", "copilot", "opencode",
            "antigravity", "crush", "maki", "cline",
        ];
        for agent in &agents {
            let tmp = tempfile::tempdir().unwrap();
            let dest = tmp.path().join(agent);
            DSbxKitEmitter::new().emit_for_agent(agent, &dest).unwrap();
            let script = dest
                .join("files")
                .join("home")
                .join(".awman")
                .join("apply-session-config.sh");
            assert!(
                script.exists(),
                "agent {agent}: startup script must exist at files/home/.awman/apply-session-config.sh"
            );
            let mode = std::fs::metadata(&script).unwrap().permissions().mode();
            assert_eq!(
                mode & 0o111,
                0o111,
                "agent {agent}: startup script must be executable (mode={mode:o})"
            );
        }
    }

    #[test]
    fn all_agents_startup_script_starts_with_shebang() {
        let agents = [
            "claude", "codex", "gemini", "copilot", "opencode",
            "antigravity", "crush", "maki", "cline",
        ];
        for agent in &agents {
            let tmp = tempfile::tempdir().unwrap();
            let dest = tmp.path().join(agent);
            DSbxKitEmitter::new().emit_for_agent(agent, &dest).unwrap();
            let script = std::fs::read_to_string(
                dest.join("files").join("home").join(".awman").join("apply-session-config.sh"),
            ).unwrap();
            assert!(
                script.starts_with("#!/"),
                "agent {agent}: startup script must start with a shebang"
            );
        }
    }
}
