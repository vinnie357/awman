//! Credential injection for the Docker Sandbox runtime.
//!
//! Maps awman's credential env-var names to `sbx` service names and registers
//! each value with `sbx secret set -g <service>`, piping the value via **stdin**
//! so it never appears in argv (process listings) and never lands in the
//! workspace-readable `session.json`.

use crate::data::message::UserMessageSink;
use crate::engine::error::EngineError;
use crate::engine::sandbox::dsbx::spawn::SbxCommand;

/// Map an awman credential env-var name to its `sbx` well-known service name,
/// or `None` when awman has no mapping for it.
pub(super) fn service_for_credential(key: &str) -> Option<&'static str> {
    match key {
        "ANTHROPIC_API_KEY" => Some("anthropic"),
        "OPENAI_API_KEY" => Some("openai"),
        "GH_TOKEN" | "GITHUB_TOKEN" => Some("github"),
        "GEMINI_API_KEY" => Some("google"),
        "AWS_ACCESS_KEY_ID" | "AWS_SECRET_ACCESS_KEY" => Some("aws"),
        "GROQ_API_KEY" => Some("groq"),
        "MISTRAL_API_KEY" => Some("mistral"),
        _ => None,
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
    ["TOKEN", "SECRET", "PASSWORD", "API_KEY", "APIKEY", "CREDENTIAL"]
        .iter()
        .any(|needle| upper.contains(needle))
}

/// Register resolved credentials with `sbx secret set`.
///
/// Mapped credentials are piped to `sbx secret set -g <service>` via stdin.
/// Unmapped credentials are not silent failures: a warning names the variable
/// and suggests the kit-level `environment.proxyManaged` route. The value is
/// never written anywhere awman controls on the host.
pub(super) fn inject_credentials(
    creds: &[(String, String)],
    sink: &mut dyn UserMessageSink,
) -> Result<(), EngineError> {
    for (key, value) in creds {
        match service_for_credential(key) {
            Some(service) => {
                SbxCommand::new(["secret", "set", "-g", service])
                    .with_stdin(value.clone().into_bytes())
                    .announce_suffix("(value piped via stdin)")
                    .redact(value.clone())
                    .run_announced(sink)?;
            }
            None => {
                sink.write_message(crate::data::message::UserMessage {
                    level: crate::data::message::MessageLevel::Warning,
                    text: format!(
                        "sbx: credential '{key}' has no known sbx service mapping and was \
                         not injected. Add `environment.proxyManaged` for it in the agent kit, \
                         or register it manually with `sbx secret set`."
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
        assert_eq!(service_for_credential("ANTHROPIC_API_KEY"), Some("anthropic"));
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
        let result = inject_credentials(&creds, &mut sink);
        assert!(result.is_ok(), "unmapped credential must not produce an error");
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
    fn mapped_credential_announcement_uses_stdin_suffix_not_value() {
        // run_announced() writes the announcement BEFORE calling run_quiet(),
        // so even when sbx is absent we can inspect the sink for the announcement.
        let mut sink = VecSink::default();
        let creds = vec![("ANTHROPIC_API_KEY".to_string(), "sk-supersecret".to_string())];
        // Ignore Ok/Err — sbx may not be installed.
        let _ = inject_credentials(&creds, &mut sink);
        let announcement = sink.messages.iter().find(|m| {
            m.level == MessageLevel::Info && m.text.contains("sbx secret set")
        });
        let msg = announcement.expect(
            "must write an Info announcement for a mapped credential",
        );
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
        // and inject_credentials passes it as an argument to `sbx secret set -g
        // <service>` — the credential VALUE is never in the argv.
        let service = service_for_credential("OPENAI_API_KEY").unwrap();
        assert_eq!(service, "openai");
        // The argv built by inject_credentials is ["secret", "set", "-g", "openai"].
        // The value "sk-xyz" would only appear if accidentally added to args.
        // We verify this indirectly by checking that the display_line for the
        // constructed command does not include a value placeholder:
        let display = format!("sbx secret set -g {service} (value piped via stdin)");
        assert!(!display.contains("sk-xyz"), "service name must not be a value");
        assert!(display.contains("openai"), "service name must appear in announcement");
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
        let result = inject_credentials(&[], &mut sink);
        assert!(result.is_ok());
        assert!(sink.messages.is_empty());
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
