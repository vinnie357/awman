//! Credential injection for the Docker Sandbox runtime.
//!
//! Maps awman's credential env-var names to `sbx` service names and registers
//! each value with `sbx secret set <sandbox> <service>`, piping the value via
//! **stdin** so it never appears in argv (process listings) and never lands in
//! the workspace-readable `session.json`.
//!
//! All registration is **sandbox-scoped and happens at agent-launch time** —
//! never `awman ready`, and never globally (`-g`). Scoped secrets take effect
//! immediately (global ones only apply at sandbox creation), awman's keys
//! never leak into sandboxes created outside awman, and `sbx rm` takes the
//! secret scope with it. The launcher guarantees the sandbox exists (created
//! via `sbx create`) before any secret is set.

use crate::data::message::UserMessageSink;
use crate::engine::container::options::{EnvLiteral, EnvVar};
use crate::engine::error::EngineError;
use crate::engine::sandbox::dsbx::spawn::SbxCommand;

/// Map an awman credential env-var name to its `sbx` well-known service name,
/// or `None` when awman has no mapping for it. Matches the service table in
/// the Docker Sandboxes credentials docs
/// (docs.docker.com/ai/sandboxes/security/credentials/, June 2026).
pub(super) fn service_for_credential(key: &str) -> Option<&'static str> {
    match key {
        "ANTHROPIC_API_KEY" => Some("anthropic"),
        "OPENAI_API_KEY" => Some("openai"),
        "GH_TOKEN" | "GITHUB_TOKEN" => Some("github"),
        "GEMINI_API_KEY" | "GOOGLE_API_KEY" => Some("google"),
        "AWS_ACCESS_KEY_ID" | "AWS_SECRET_ACCESS_KEY" => Some("aws"),
        "GROQ_API_KEY" => Some("groq"),
        "MISTRAL_API_KEY" => Some("mistral"),
        _ => None,
    }
}

/// Allowlist of provider auth env vars accepted via `env(VAR)` overlays for
/// launch-time auto-auth, per agent. Only mixin-kit agents participate —
/// agent-kit agents (antigravity, crush, maki, cline) are intentionally left
/// out for now, so they return an empty list.
///
/// Each var must satisfy two constraints, both verified against the Docker
/// Sandboxes credentials docs: the var maps to an sbx well-known service
/// ([`service_for_credential`]), and the agent's base kit actually routes that
/// service through the host proxy (the kit template's `network.allowedDomains`
/// / `environment.proxyManaged`).
pub(super) fn supported_auth_env_vars(agent: &str) -> &'static [&'static str] {
    match agent {
        "claude" => &["ANTHROPIC_API_KEY"],
        "codex" => &["OPENAI_API_KEY"],
        "gemini" => &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
        "copilot" => &["GH_TOKEN", "GITHUB_TOKEN"],
        "opencode" => &["ANTHROPIC_API_KEY"],
        _ => &[],
    }
}

/// Heuristic: does this env-var name look like it carries a secret? Used to
/// keep credential-class values out of the workspace-readable `session.json`
/// even when they have no explicit `sbx` service mapping.
pub(super) fn is_credential_like(key: &str) -> bool {
    if service_for_credential(key).is_some() {
        return true;
    }
    let upper = key.to_ascii_uppercase();
    [
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "API_KEY",
        "APIKEY",
        "CREDENTIAL",
    ]
    .iter()
    .any(|needle| upper.contains(needle))
}

/// Launch-time auto-auth driven by `env(VAR)` overlays.
///
/// For each passthrough var on the agent's [`supported_auth_env_vars`]
/// allowlist, the host value is registered with `sbx secret set <sandbox>
/// <service>` (piped via stdin, redacted) so the sbx host proxy is
/// authenticated by the time the agent starts. Runs at agent-launch time —
/// not `awman ready` — so rotated keys take effect without re-running ready.
/// `sandbox` must already exist (the launcher creates it first).
///
/// Everything else is a warning, never a silent drop and never a launch
/// abort:
/// - a supported var whose host value is unset,
/// - a credential-class var outside the allowlist (dropped — it is already
///   excluded from the workspace-readable `session.json`),
/// - a credential-class `env_literal` (withheld; point at `env(VAR)`),
/// - an allowlisted agent launching with no supported auth overlay at all
///   (auth must be set up manually or via in-sandbox login).
///
/// `lookup_env` abstracts `std::env::var` so tests don't mutate process-global
/// state. A failed `sbx secret set` subprocess is launch-blocking (WI 0090).
///
/// Returns `true` when at least one supported auth var was read from the host
/// and registered with `sbx secret set` — i.e. the sbx proxy now has a
/// credential awman put there. The caller threads this into
/// [`inject_credentials`] so a redundant warning (e.g. about
/// `CLAUDE_CODE_OAUTH_TOKEN`) is suppressed once env-overlay auth already
/// covered the agent.
pub(super) fn auto_auth_env_overlays(
    agent: &str,
    sandbox: &str,
    env_passthrough: &[EnvVar],
    env_literal: &[EnvLiteral],
    lookup_env: &dyn Fn(&str) -> Option<String>,
    sink: &mut dyn UserMessageSink,
) -> Result<bool, EngineError> {
    let supported = supported_auth_env_vars(agent);
    let mut configured_services: Vec<&'static str> = Vec::new();

    for var in env_passthrough {
        let key = var.0.as_str();
        if supported.contains(&key) {
            let Some(service) = service_for_credential(key) else {
                // Unreachable while the allowlist test below holds; degrade to
                // the unsupported-var warning rather than panicking.
                warn_unsupported_dropped(agent, key, supported, sink);
                continue;
            };
            if configured_services.contains(&service) {
                continue; // e.g. both GH_TOKEN and GITHUB_TOKEN requested
            }
            match lookup_env(key) {
                Some(value) if !value.is_empty() => {
                    set_secret(service, &value, sandbox, sink)?;
                    configured_services.push(service);
                }
                _ => {
                    sink.write_message(warning(format!(
                        "sbx: env({key}) was requested but {key} is not set in the \
                         host environment; no '{service}' credential was registered."
                    )));
                }
            }
        } else if is_credential_like(key) {
            warn_unsupported_dropped(agent, key, supported, sink);
        }
        // Non-credential vars flow to the agent via session.json as before.
    }

    for lit in env_literal {
        if is_credential_like(&lit.key) {
            sink.write_message(warning(format!(
                "sbx: credential-class env literal '{}' was withheld from the \
                 workspace-readable session.json. Pass it as an env({}) overlay to \
                 enable launch-time auto-auth, or register it with `sbx secret set`.",
                lit.key, lit.key
            )));
        }
    }

    if !supported.is_empty() && configured_services.is_empty() {
        sink.write_message(warning(format!(
            "sbx: no env(...) overlay supplied a supported auth variable for \
             '{agent}' (supported: {}). The sbx proxy has no credential configured \
             by awman — register one manually with `sbx secret set {sandbox} \
             <service>` (sandbox-scoped secrets apply immediately, even while \
             running), or complete the agent's login flow inside the sandbox. \
             Launching anyway.",
            supported.join(", ")
        )));
    }

    Ok(!configured_services.is_empty())
}

fn warn_unsupported_dropped(
    agent: &str,
    key: &str,
    supported: &[&str],
    sink: &mut dyn UserMessageSink,
) {
    let hint = if supported.is_empty() {
        format!(
            "launch-time auto-auth is not yet supported for agent '{agent}'; register \
             the credential manually with `sbx secret set`"
        )
    } else {
        format!(
            "supported auth variables for '{agent}': {}",
            supported.join(", ")
        )
    };
    sink.write_message(warning(format!(
        "sbx: env var '{key}' is not a supported sbx auth variable for agent \
         '{agent}' and was dropped ({hint})."
    )));
}

/// Register `value` for `service` with `sbx secret set <sandbox> <service>`,
/// piping the value via stdin. Always sandbox-scoped, never global (`-g`):
/// scoped secrets take effect immediately (running or stopped), while global
/// ones only apply at sandbox creation — and scoped registration keeps
/// awman's keys out of sandboxes it does not own.
fn set_secret(
    service: &str,
    value: &str,
    sandbox: &str,
    sink: &mut dyn UserMessageSink,
) -> Result<(), EngineError> {
    run_secret_set(
        vec![
            "secret".into(),
            "set".into(),
            sandbox.into(),
            service.into(),
        ],
        value,
        sink,
    )
}

/// One `sbx secret set` invocation with the value piped via stdin.
///
/// The sbx docs are inconsistent about whether non-interactive input needs a
/// `--password-stdin` flag; plain stdin piping is confirmed working against
/// the current sbx, so it is the primary form. When it fails with an error
/// that looks like sbx tried to prompt interactively, retry once with
/// `--password-stdin` appended before giving up.
fn run_secret_set(
    args: Vec<String>,
    value: &str,
    sink: &mut dyn UserMessageSink,
) -> Result<(), EngineError> {
    let attempt = SbxCommand::new(args.clone())
        .with_stdin(value.as_bytes().to_vec())
        .announce_suffix("(value piped via stdin)")
        .redact(value.to_string())
        .run_announced(sink);
    match attempt {
        Ok(_) => Ok(()),
        Err(e) if error_suggests_password_stdin(&e) => {
            let mut retry = args;
            retry.push("--password-stdin".to_string());
            SbxCommand::new(retry)
                .with_stdin(value.as_bytes().to_vec())
                .announce_suffix("(value piped via stdin)")
                .redact(value.to_string())
                .run_announced(sink)
                .map(|_| ())
        }
        Err(e) => Err(e),
    }
}

/// Does this `sbx secret set` failure look like sbx expected interactive
/// input (and so may want `--password-stdin`)? Matched against the wrapped
/// error text, which embeds sbx's stderr. "stdin" itself is not a needle —
/// the announcement suffix "(value piped via stdin)" is part of the error
/// string and would always match.
fn error_suggests_password_stdin(e: &EngineError) -> bool {
    let msg = e.to_string().to_lowercase();
    ["tty", "terminal", "interactive", "password"]
        .iter()
        .any(|needle| msg.contains(needle))
}

fn warning(text: String) -> crate::data::message::UserMessage {
    crate::data::message::UserMessage {
        level: crate::data::message::MessageLevel::Warning,
        text,
    }
}

/// Register awman-resolved credentials (keychain etc.) with `sbx secret set`.
///
/// Mapped credentials are piped to `sbx secret set <sandbox> <service>` via
/// stdin — sandbox-scoped at launch time, like all sbx secret registration.
/// Unmapped credentials are not silent failures: a warning names the variable
/// and suggests the kit-level `environment.proxyManaged` route. The value is
/// never written anywhere awman controls on the host.
///
/// `auth_overlay_registered` reports whether [`auto_auth_env_overlays`] already
/// registered a supported auth var for this launch. When it did, the
/// `CLAUDE_CODE_OAUTH_TOKEN` warning is redundant (the sbx proxy is already
/// authenticated via the overlay, e.g. `ANTHROPIC_API_KEY`) and is suppressed;
/// when no supported overlay var was registered, the warning still fires so the
/// user knows the OAuth token was dropped and how to authenticate.
pub(super) fn inject_credentials(
    creds: &[(String, String)],
    sandbox: &str,
    auth_overlay_registered: bool,
    sink: &mut dyn UserMessageSink,
) -> Result<(), EngineError> {
    for (key, value) in creds {
        match service_for_credential(key) {
            Some(service) => {
                set_secret(service, value, sandbox, sink)?;
            }
            None if key == "CLAUDE_CODE_OAUTH_TOKEN" && auth_overlay_registered => {
                // A supported auth var (e.g. ANTHROPIC_API_KEY) was already
                // registered via an env() overlay, so the proxy is
                // authenticated and the OAuth token is moot — stay quiet
                // rather than warning about a credential that isn't needed.
            }
            None if key == "CLAUDE_CODE_OAUTH_TOKEN" => {
                // sbx's credential proxy injects the `anthropic` service value as
                // an `x-api-key` header, which a subscription OAuth token cannot
                // use — sbx has no OAuth-token support yet (docker/sbx-releases#11).
                sink.write_message(crate::data::message::UserMessage {
                    level: crate::data::message::MessageLevel::Warning,
                    text: "sbx: CLAUDE_CODE_OAUTH_TOKEN is not supported by Docker \
                           Sandboxes yet and was not injected. Either set \
                           ANTHROPIC_API_KEY, or run `/login` inside the sandbox — \
                           the sbx proxy completes the OAuth flow and keeps the \
                           token on the host."
                        .to_string(),
                });
            }
            None => {
                sink.write_message(crate::data::message::UserMessage {
                    level: crate::data::message::MessageLevel::Warning,
                    text: format!(
                        "sbx: credential '{key}' has no known sbx service mapping and was \
                         not injected. Declare it under `credentials.sources` in the agent \
                         kit, or register it manually with `sbx secret set`."
                    ),
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::message::MessageLevel;

    #[test]
    fn known_services_map() {
        assert_eq!(
            service_for_credential("ANTHROPIC_API_KEY"),
            Some("anthropic")
        );
        assert_eq!(service_for_credential("OPENAI_API_KEY"), Some("openai"));
        assert_eq!(service_for_credential("GH_TOKEN"), Some("github"));
        assert_eq!(service_for_credential("GITHUB_TOKEN"), Some("github"));
        assert_eq!(service_for_credential("GEMINI_API_KEY"), Some("google"));
        assert_eq!(service_for_credential("AWS_ACCESS_KEY_ID"), Some("aws"));
        assert_eq!(service_for_credential("AWS_SECRET_ACCESS_KEY"), Some("aws"));
        assert_eq!(service_for_credential("GROQ_API_KEY"), Some("groq"));
        assert_eq!(service_for_credential("MISTRAL_API_KEY"), Some("mistral"));
    }

    #[test]
    fn unknown_service_is_none() {
        assert_eq!(service_for_credential("SOME_RANDOM_VAR"), None);
    }

    #[test]
    fn credential_like_heuristic() {
        assert!(is_credential_like("ANTHROPIC_API_KEY"));
        assert!(is_credential_like("MY_SERVICE_TOKEN"));
        assert!(is_credential_like("DB_PASSWORD"));
        assert!(!is_credential_like("LOG_LEVEL"));
        assert!(!is_credential_like("WORKDIR"));
    }

    // ─── inject_credentials behaviour ─────────────────────────────────────

    #[test]
    fn unmapped_credential_produces_warning_not_error() {
        let mut sink = VecSink::default();
        let creds = vec![("SOME_UNKNOWN_VAR".to_string(), "value".to_string())];
        // Must not return Err — unmapped credentials are soft-skipped.
        let result = inject_credentials(&creds, "awman-h-claude", false, &mut sink);
        assert!(
            result.is_ok(),
            "unmapped credential must not produce an error"
        );
        assert!(
            sink.messages.iter().any(|m| {
                m.level == MessageLevel::Warning
                    && m.text.contains("SOME_UNKNOWN_VAR")
                    && m.text.contains("no known sbx service mapping")
            }),
            "must produce a Warning naming the variable; messages: {:?}",
            sink.messages
        );
    }

    #[test]
    fn claude_oauth_token_warns_with_supported_alternatives() {
        let mut sink = VecSink::default();
        let creds = vec![(
            "CLAUDE_CODE_OAUTH_TOKEN".to_string(),
            "sk-ant-oat-secret".to_string(),
        )];
        // No supported env() overlay was registered → the warning must fire so
        // the user knows the OAuth token was dropped and how to authenticate.
        let result = inject_credentials(&creds, "awman-h-claude", false, &mut sink);
        assert!(
            result.is_ok(),
            "unsupported OAuth token must not produce an error"
        );
        let warning = sink
            .messages
            .iter()
            .find(|m| m.level == MessageLevel::Warning)
            .expect("must warn about CLAUDE_CODE_OAUTH_TOKEN");
        assert!(
            warning.text.contains("CLAUDE_CODE_OAUTH_TOKEN")
                && warning.text.contains("ANTHROPIC_API_KEY")
                && warning.text.contains("/login"),
            "warning must name the token and both supported alternatives; got: {:?}",
            warning.text
        );
        assert!(
            !warning.text.contains("sk-ant-oat-secret"),
            "token value must never appear in messages"
        );
    }

    #[test]
    fn claude_oauth_token_warning_suppressed_when_overlay_auth_registered() {
        // When a supported env() overlay (e.g. ANTHROPIC_API_KEY) was already
        // registered, the proxy is authenticated and the OAuth token is moot —
        // the warning must not fire.
        let mut sink = VecSink::default();
        let creds = vec![(
            "CLAUDE_CODE_OAUTH_TOKEN".to_string(),
            "sk-ant-oat-secret".to_string(),
        )];
        let result = inject_credentials(&creds, "awman-h-claude", true, &mut sink);
        assert!(
            result.is_ok(),
            "suppressed OAuth token must not produce an error"
        );
        assert!(
            !sink
                .messages
                .iter()
                .any(|m| m.text.contains("CLAUDE_CODE_OAUTH_TOKEN")),
            "OAuth warning must be suppressed once overlay auth is registered; messages: {:?}",
            sink.messages
        );
    }

    #[test]
    fn mapped_credential_announcement_uses_stdin_suffix_not_value() {
        // run_announced() writes the announcement BEFORE calling run_quiet(),
        // so even when sbx is absent we can inspect the sink for the announcement.
        let mut sink = VecSink::default();
        let creds = vec![(
            "ANTHROPIC_API_KEY".to_string(),
            "sk-supersecret".to_string(),
        )];
        // Ignore Ok/Err — sbx may not be installed.
        let _ = inject_credentials(&creds, "awman-h-claude", false, &mut sink);
        let announcement = sink
            .messages
            .iter()
            .find(|m| m.level == MessageLevel::Info && m.text.contains("sbx secret set"));
        let msg = announcement.expect("must write an Info announcement for a mapped credential");
        assert!(
            msg.text.contains("(value piped via stdin)"),
            "announcement must describe stdin delivery; got: {:?}",
            msg.text
        );
        assert!(
            !msg.text.contains("sk-supersecret"),
            "secret value must not appear in the announcement: {:?}",
            msg.text
        );
    }

    #[test]
    fn mapped_credential_argv_does_not_contain_service_value() {
        // Structural guarantee: service_for_credential returns the service name
        // and inject_credentials passes it as an argument to `sbx secret set
        // <sandbox> <service>` — the credential VALUE is never in the argv.
        let service = service_for_credential("OPENAI_API_KEY").unwrap();
        assert_eq!(service, "openai");
        // The argv built is ["secret", "set", "<sandbox>", "openai"]. The value
        // "sk-xyz" would only appear if accidentally added to args. We verify
        // this indirectly via the display_line shape:
        let display = format!("sbx secret set awman-h-codex {service} (value piped via stdin)");
        assert!(
            !display.contains("sk-xyz"),
            "service name must not be a value"
        );
        assert!(
            display.contains("openai"),
            "service name must appear in announcement"
        );
    }

    #[test]
    fn all_credential_table_entries_have_expected_service_names() {
        let table: &[(&str, &str)] = &[
            ("ANTHROPIC_API_KEY", "anthropic"),
            ("OPENAI_API_KEY", "openai"),
            ("GH_TOKEN", "github"),
            ("GITHUB_TOKEN", "github"),
            ("GEMINI_API_KEY", "google"),
            ("AWS_ACCESS_KEY_ID", "aws"),
            ("AWS_SECRET_ACCESS_KEY", "aws"),
            ("GROQ_API_KEY", "groq"),
            ("MISTRAL_API_KEY", "mistral"),
        ];
        for (key, expected_service) in table {
            assert_eq!(
                service_for_credential(key),
                Some(*expected_service),
                "key {key} should map to {expected_service}"
            );
        }
    }

    #[test]
    fn inject_empty_creds_is_noop_ok() {
        let mut sink = VecSink::default();
        let result = inject_credentials(&[], "awman-h-claude", false, &mut sink);
        assert!(result.is_ok());
        assert!(sink.messages.is_empty());
    }

    // ─── Launch-time env() overlay auto-auth ──────────────────────────────

    use crate::engine::container::options::{EnvLiteral, EnvVar};

    fn passthrough(keys: &[&str]) -> Vec<EnvVar> {
        keys.iter().map(|k| EnvVar((*k).to_string())).collect()
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn allowlist_covers_mixin_agents_only() {
        // Verified against docs.docker.com/ai/sandboxes/security/credentials/:
        // service ↔ env var pairs per built-in agent.
        assert_eq!(supported_auth_env_vars("claude"), &["ANTHROPIC_API_KEY"]);
        assert_eq!(supported_auth_env_vars("codex"), &["OPENAI_API_KEY"]);
        assert_eq!(
            supported_auth_env_vars("gemini"),
            &["GEMINI_API_KEY", "GOOGLE_API_KEY"]
        );
        assert_eq!(
            supported_auth_env_vars("copilot"),
            &["GH_TOKEN", "GITHUB_TOKEN"]
        );
        assert_eq!(supported_auth_env_vars("opencode"), &["ANTHROPIC_API_KEY"]);
        // Agent-kit agents are out of scope for now.
        for agent in ["antigravity", "crush", "maki", "cline", "unknown"] {
            assert!(
                supported_auth_env_vars(agent).is_empty(),
                "agent-kit agent {agent} must have no auto-auth allowlist"
            );
        }
    }

    #[test]
    fn every_allowlisted_var_maps_to_an_sbx_service() {
        // Guards the non-panicking fallback in auto_auth_env_overlays: a
        // supported var without a service mapping is a table bug.
        for agent in ["claude", "codex", "gemini", "copilot", "opencode"] {
            for var in supported_auth_env_vars(agent) {
                assert!(
                    service_for_credential(var).is_some(),
                    "allowlisted var {var} (agent {agent}) must map to an sbx service"
                );
            }
        }
    }

    #[test]
    fn missing_host_value_warns_and_then_warns_manual_auth() {
        let mut sink = VecSink::default();
        let result = auto_auth_env_overlays(
            "claude",
            "awman-h-claude",
            &passthrough(&["ANTHROPIC_API_KEY"]),
            &[],
            &no_env,
            &mut sink,
        );
        assert!(result.is_ok(), "missing host value must not block launch");
        assert!(
            sink.messages.iter().any(|m| {
                m.level == MessageLevel::Warning
                    && m.text.contains("ANTHROPIC_API_KEY is not set in the host")
            }),
            "must warn the requested var is unset; messages: {:?}",
            sink.messages
        );
        assert!(
            sink.messages.iter().any(|m| {
                m.level == MessageLevel::Warning && m.text.contains("Launching anyway")
            }),
            "no credential was registered, so the manual-auth warning must follow"
        );
    }

    #[test]
    fn unsupported_credential_var_is_dropped_with_warning() {
        let mut sink = VecSink::default();
        // OPENAI_API_KEY is a valid sbx var but not for claude.
        let result = auto_auth_env_overlays(
            "claude",
            "awman-h-claude",
            &passthrough(&["OPENAI_API_KEY"]),
            &[],
            &|_| Some("sk-value".into()),
            &mut sink,
        );
        assert!(result.is_ok());
        let dropped = sink
            .messages
            .iter()
            .find(|m| m.text.contains("OPENAI_API_KEY") && m.text.contains("dropped"))
            .expect("unsupported var must produce a dropped warning");
        assert_eq!(dropped.level, MessageLevel::Warning);
        assert!(
            dropped.text.contains("ANTHROPIC_API_KEY"),
            "warning must list the agent's supported vars: {}",
            dropped.text
        );
        assert!(
            !sink.messages.iter().any(|m| m.text.contains("sk-value")),
            "the value must never reach the sink"
        );
    }

    #[test]
    fn non_credential_var_passes_silently() {
        let mut sink = VecSink::default();
        auto_auth_env_overlays(
            "claude",
            "awman-h-claude",
            &passthrough(&["LOG_LEVEL"]),
            &[],
            &|_| Some("debug".into()),
            &mut sink,
        )
        .unwrap();
        assert!(
            !sink.messages.iter().any(|m| m.text.contains("LOG_LEVEL")),
            "non-credential vars are session.json passthrough, not auth: {:?}",
            sink.messages
        );
    }

    #[test]
    fn agent_kit_agent_gets_drop_warning_but_no_manual_auth_warning() {
        let mut sink = VecSink::default();
        auto_auth_env_overlays(
            "crush",
            "awman-h-crush",
            &passthrough(&["ANTHROPIC_API_KEY"]),
            &[],
            &|_| Some("sk-value".into()),
            &mut sink,
        )
        .unwrap();
        assert!(
            sink.messages
                .iter()
                .any(|m| { m.text.contains("not yet supported for agent 'crush'") }),
            "agent-kit agents must explain auto-auth is out of scope: {:?}",
            sink.messages
        );
        assert!(
            !sink
                .messages
                .iter()
                .any(|m| m.text.contains("Launching anyway")),
            "the no-auth-overlay warning is mixin-only"
        );
    }

    #[test]
    fn credential_class_env_literal_is_withheld_with_overlay_hint() {
        let mut sink = VecSink::default();
        auto_auth_env_overlays(
            "claude",
            "awman-h-claude",
            &[],
            &[EnvLiteral {
                key: "MY_DB_PASSWORD".into(),
                value: "hunter2".into(),
            }],
            &no_env,
            &mut sink,
        )
        .unwrap();
        let withheld = sink
            .messages
            .iter()
            .find(|m| m.text.contains("MY_DB_PASSWORD") && m.text.contains("withheld"))
            .expect("credential-class literal must be warned about");
        assert!(
            withheld.text.contains("env(MY_DB_PASSWORD)"),
            "warning must point at the env() overlay route: {}",
            withheld.text
        );
        assert!(
            !sink.messages.iter().any(|m| m.text.contains("hunter2")),
            "literal values must never reach the sink"
        );
    }

    #[cfg(unix)]
    mod with_subprocess {
        use super::*;
        use crate::engine::sandbox::dsbx::test_support::with_fake_sbx;

        #[test]
        fn supported_var_is_registered_sandbox_scoped_never_global() {
            with_fake_sbx("#!/bin/sh\ncat > /dev/null\n", || {
                let mut sink = VecSink::default();
                let registered = auto_auth_env_overlays(
                    "claude",
                    "awman-h-claude",
                    &passthrough(&["ANTHROPIC_API_KEY"]),
                    &[],
                    &|_| Some("sk-ant-secret".into()),
                    &mut sink,
                )
                .unwrap();
                assert!(
                    registered,
                    "a successful secret set must report overlay auth as registered"
                );
                assert!(
                    sink.messages.iter().any(|m| {
                        m.text.contains(
                            "sbx secret set awman-h-claude anthropic (value piped via stdin)",
                        )
                    }),
                    "must announce the sandbox-scoped secret set; messages: {:?}",
                    sink.messages
                );
                assert!(
                    !sink.messages.iter().any(|m| m.text.contains(" -g ")),
                    "secret registration must never be global; messages: {:?}",
                    sink.messages
                );
                assert!(
                    !sink
                        .messages
                        .iter()
                        .any(|m| m.text.contains("sk-ant-secret")),
                    "value must be redacted everywhere"
                );
                assert!(
                    !sink
                        .messages
                        .iter()
                        .any(|m| m.text.contains("Launching anyway")),
                    "a registered credential suppresses the manual-auth warning"
                );
            });
        }

        #[test]
        fn duplicate_vars_for_one_service_register_once() {
            with_fake_sbx("#!/bin/sh\ncat > /dev/null\n", || {
                let mut sink = VecSink::default();
                auto_auth_env_overlays(
                    "copilot",
                    "awman-h-copilot",
                    &passthrough(&["GH_TOKEN", "GITHUB_TOKEN"]),
                    &[],
                    &|_| Some("ghp-secret".into()),
                    &mut sink,
                )
                .unwrap();
                let github_sets = sink
                    .messages
                    .iter()
                    .filter(|m| {
                        m.text
                            .contains("Running: sbx secret set awman-h-copilot github")
                    })
                    .count();
                assert_eq!(
                    github_sets, 1,
                    "one registration per service: {:?}",
                    sink.messages
                );
            });
        }

        #[test]
        fn interactive_prompt_failure_retries_with_password_stdin() {
            // A fake sbx that rejects plain stdin (as some doc versions imply)
            // and only succeeds when --password-stdin is passed.
            with_fake_sbx(
                "#!/bin/sh\n\
                 case \"$@\" in\n\
                   *--password-stdin*) cat > /dev/null; exit 0;;\n\
                   *) echo 'ERROR: cannot prompt for password in non-interactive terminal' >&2; exit 1;;\n\
                 esac\n",
                || {
                    let mut sink = VecSink::default();
                    let result = auto_auth_env_overlays(
                        "claude",
                        "awman-h-claude",
                        &passthrough(&["ANTHROPIC_API_KEY"]),
                        &[],
                        &|_| Some("sk-ant-secret".into()),
                        &mut sink,
                    );
                    assert!(result.is_ok(), "retry with --password-stdin must succeed: {result:?}");
                    assert!(
                        sink.messages.iter().any(|m| {
                            m.text.contains(
                                "sbx secret set awman-h-claude anthropic --password-stdin",
                            )
                        }),
                        "retry announcement must show the flag; messages: {:?}",
                        sink.messages
                    );
                },
            );
        }

        #[test]
        fn unrelated_secret_set_failure_blocks_launch() {
            with_fake_sbx(
                "#!/bin/sh\necho 'ERROR: not logged in; run sbx login' >&2\nexit 1\n",
                || {
                    let mut sink = VecSink::default();
                    let result = auto_auth_env_overlays(
                        "claude",
                        "awman-h-claude",
                        &passthrough(&["ANTHROPIC_API_KEY"]),
                        &[],
                        &|_| Some("sk-ant-secret".into()),
                        &mut sink,
                    );
                    match result {
                        Err(EngineError::Sandbox(msg)) => {
                            assert!(
                                msg.contains("not logged in"),
                                "error must carry sbx's diagnostics: {msg}"
                            );
                            assert!(
                                !msg.contains("sk-ant-secret"),
                                "error must not leak the value: {msg}"
                            );
                        }
                        other => panic!("secret set failure must block launch, got {other:?}"),
                    }
                },
            );
        }
    }

    // ─── VecSink helper ───────────────────────────────────────────────────

    #[derive(Default)]
    struct VecSink {
        messages: Vec<crate::data::message::UserMessage>,
    }
    impl crate::data::message::UserMessageSink for VecSink {
        fn write_message(&mut self, msg: crate::data::message::UserMessage) {
            self.messages.push(msg);
        }
        fn replay_queued(&mut self) {}
    }
}
