//! Layer 0 template inclusions — embedded Dockerfile templates and audit
//! prompts. These exist as a single Layer-0 module so callers everywhere
//! can resolve the templates without having to redefine paths.

pub mod audit_prompts;

pub use audit_prompts::{init_audit_prompt, ready_audit_prompt};

/// The project base Dockerfile. Written by `awman init` and `awman ready`
/// when no `Dockerfile.dev` exists at the git root.
pub fn project_dockerfile_dev() -> &'static str {
    include_str!("../../../templates/Dockerfile.project")
}

/// Per-agent Dockerfile template (fallback when network download fails).
pub fn agent_dockerfile_for(agent: &str) -> Option<&'static str> {
    Some(match agent {
        "claude" => include_str!("../../../templates/Dockerfile.claude"),
        "codex" => include_str!("../../../templates/Dockerfile.codex"),
        "opencode" => include_str!("../../../templates/Dockerfile.opencode"),
        "maki" => include_str!("../../../templates/Dockerfile.maki"),
        "gemini" => include_str!("../../../templates/Dockerfile.gemini"),
        "copilot" => include_str!("../../../templates/Dockerfile.copilot"),
        "crush" => include_str!("../../../templates/Dockerfile.crush"),
        "cline" => include_str!("../../../templates/Dockerfile.cline"),
        "agy" | "antigravity" => include_str!("../../../templates/Dockerfile.antigravity"),
        _ => return None,
    })
}

/// Returns `true` when the given content matches the bundled project base
/// template (ignoring leading/trailing whitespace). Used by the ready engine
/// to decide whether an audit should be offered.
pub fn dockerfile_matches_template(content: &str) -> bool {
    content.trim() == project_dockerfile_dev().trim()
}

/// Per-agent Docker Sandbox kit YAML template (`spec.yaml` source).
///
/// Additive sibling of [`agent_dockerfile_for`] — the sbx kit templates live
/// alongside the Dockerfile templates in `templates/` and are embedded with
/// the same `include_str!` mechanism. A caller that wants a Dockerfile calls
/// `agent_dockerfile_for`; a caller that wants a kit calls this. The two
/// families never collide because of the `sbx-kit.` filename prefix.
pub fn sbx_kit_template_for(agent: &str) -> Option<&'static str> {
    Some(match agent {
        "claude" => include_str!("../../../templates/sbx-kit.claude.yaml"),
        "codex" => include_str!("../../../templates/sbx-kit.codex.yaml"),
        "opencode" => include_str!("../../../templates/sbx-kit.opencode.yaml"),
        "maki" => include_str!("../../../templates/sbx-kit.maki.yaml"),
        "gemini" => include_str!("../../../templates/sbx-kit.gemini.yaml"),
        "copilot" => include_str!("../../../templates/sbx-kit.copilot.yaml"),
        "crush" => include_str!("../../../templates/sbx-kit.crush.yaml"),
        "cline" => include_str!("../../../templates/sbx-kit.cline.yaml"),
        "agy" | "antigravity" => include_str!("../../../templates/sbx-kit.antigravity.yaml"),
        _ => return None,
    })
}

/// Per-agent Docker Sandbox startup script (`apply-session-config.sh` source).
///
/// Written into the emitted kit's `files/home/.awman/` directory by the kit
/// emitter at `awman ready` time and re-run by the kit's `commands.startup`
/// on every sandbox restart.
pub fn sbx_apply_script_for(agent: &str) -> Option<&'static str> {
    Some(match agent {
        "claude" => include_str!("../../../templates/sbx-apply.claude.sh"),
        "codex" => include_str!("../../../templates/sbx-apply.codex.sh"),
        "opencode" => include_str!("../../../templates/sbx-apply.opencode.sh"),
        "maki" => include_str!("../../../templates/sbx-apply.maki.sh"),
        "gemini" => include_str!("../../../templates/sbx-apply.gemini.sh"),
        "copilot" => include_str!("../../../templates/sbx-apply.copilot.sh"),
        "crush" => include_str!("../../../templates/sbx-apply.crush.sh"),
        "cline" => include_str!("../../../templates/sbx-apply.cline.sh"),
        "agy" | "antigravity" => include_str!("../../../templates/sbx-apply.antigravity.sh"),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agy_uses_the_existing_legacy_named_assets_for_both_input_spellings() {
        assert_eq!(
            agent_dockerfile_for("agy"),
            agent_dockerfile_for("antigravity")
        );
        assert_eq!(
            sbx_kit_template_for("agy"),
            sbx_kit_template_for("antigravity")
        );
        assert_eq!(
            sbx_apply_script_for("agy"),
            sbx_apply_script_for("antigravity")
        );
        assert!(agent_dockerfile_for("agy").is_some());
        assert!(sbx_kit_template_for("agy").is_some());
        assert!(sbx_apply_script_for("agy").is_some());
    }

    #[test]
    fn unknown_agent_has_no_legacy_asset_mapping() {
        assert!(agent_dockerfile_for("actualagy").is_none());
        assert!(sbx_kit_template_for("actualagy").is_none());
        assert!(sbx_apply_script_for("actualagy").is_none());
    }

    const CODEX_DISCRIMINATOR: &str = r#"p="$(readlink -f "$(command -v codex)")"; [ "$(basename "$(dirname "$p")")" = bin ] && test -r "$(dirname "$p")/../codex-package.json""#;

    fn codex_template() -> &'static str {
        agent_dockerfile_for("codex").expect("codex template exists")
    }

    /// Template lines with `#` comment lines removed.
    fn codex_template_code_lines() -> Vec<&'static str> {
        codex_template()
            .lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .collect()
    }

    #[test]
    fn codex_template_does_not_copy_codex_binary_into_usr_local_bin() {
        for l in codex_template_code_lines() {
            let tokens: Vec<&str> = l
                .split_whitespace()
                .map(|t| t.trim_matches(|c| c == '"' || c == '\''))
                .collect();
            let copies = tokens.contains(&"install") || tokens.contains(&"cp");
            let targets_codex = tokens.contains(&"/usr/local/bin/codex");
            assert!(
                !(copies && (targets_codex || l.contains("codex_bin"))),
                "codex template must keep the installer symlink, not copy the binary: {l}"
            );
        }
    }

    #[test]
    fn codex_template_installs_with_codex_home_opt_codex() {
        let code = codex_template_code_lines().join("\n");
        assert!(
            code.contains("CODEX_HOME=/opt/codex"),
            "install must set CODEX_HOME=/opt/codex"
        );
        assert!(
            code.contains("CODEX_INSTALL_DIR=/usr/local/bin"),
            "install must set CODEX_INSTALL_DIR=/usr/local/bin"
        );
        assert!(
            code.contains("chmod -R a+rX /opt/codex"),
            "package tree must be world-readable"
        );
    }

    #[test]
    fn codex_template_has_no_runtime_codex_home_env() {
        let mut in_env = false;
        for l in codex_template_code_lines() {
            let t = l.trim();
            if t.starts_with("ENV") {
                in_env = true;
            }
            assert!(
                !(in_env && t.contains("CODEX_HOME")),
                "runtime CODEX_HOME must stay default: {l}"
            );
            if !t.ends_with('\\') {
                in_env = false;
            }
        }
    }

    #[test]
    fn codex_template_smoke_runs_package_layout_discriminator_as_awman() {
        let t = codex_template();
        let user = t
            .find("USER awman")
            .expect("template must switch to USER awman");
        let smoke = t
            .find(CODEX_DISCRIMINATOR)
            .expect("smoke must run the package-layout discriminator");
        assert!(smoke > user, "discriminator must run after USER awman");
    }
}
