//! Issue 230: bounded native SDK acknowledgement observation, independent of publication.
//! Schemas: Altana council_headless_protocol.zig:433–449, 566–574, 775–811.
//! The cfg(test) seam feeds the real observer and returns session/completed diagnostics.
//! peak_retained_bytes counts peak internal allocation capacities while feeding input.
//! It excludes caller chunks, transient allocations and process RSS; it is not malloc peak.

use super::native_sdk_execution::observe_sdk_chunks;
use std::error::Error;

type TestResult = Result<(), Box<dyn Error>>;
const AGENTS: [&str; 3] = ["claude", "codex", "agy"];
const SESSION: &str = "native-observer-session";
const RETAINED_BOUND: usize = 32 * 1024;

fn acknowledgement(agent: &str, encoded_id: &str) -> String {
    match agent {
        "claude" => format!(
            "{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"session_id\":\"{encoded_id}\",\"result\":\"review λ\"}}\n"
        ),
        "codex" => format!(
            "{{\"type\":\"thread.started\",\"thread_id\":\"{encoded_id}\"}}\n{{\"type\":\"turn.started\"}}\n{{\"type\":\"item.completed\",\"item\":{{\"type\":\"agent_message\",\"text\":\"review λ\"}}}}\n{{\"type\":\"turn.completed\",\"usage\":{{\"input_tokens\":1,\"cached_input_tokens\":0,\"output_tokens\":1}}}}\n"
        ),
        "agy" => format!(
            "{{\"status\":\"SUCCESS\",\"conversation_id\":\"{encoded_id}\",\"response\":\"review λ\"}}\n"
        ),
        _ => unreachable!("test provider must be canonical"),
    }
}

fn identity_key(agent: &str) -> &str {
    match agent {
        "claude" => "session_id",
        "codex" => "thread_id",
        "agy" => "conversation_id",
        _ => unreachable!("test provider must be canonical"),
    }
}

fn cannot_publish(agent: &str, bytes: &[u8]) {
    let chunks: Vec<_> = bytes.chunks(7).collect();
    if let Ok(observed) = observe_sdk_chunks(agent, None, &chunks, true) {
        assert!(
            observed.session.is_none() || !observed.completed,
            "{agent} established an acknowledgement from invalid identity evidence"
        );
        assert!(observed.peak_retained_bytes <= RETAINED_BOUND);
    }
}

fn rejects(agent: &str, bytes: &[u8]) {
    let chunks: Vec<_> = bytes.chunks(3).collect();
    assert!(
        observe_sdk_chunks(agent, None, &chunks, true).is_err(),
        "{agent} accepted invalid acknowledgement input"
    );
}

#[test]
fn empty_and_unrelated_output_cannot_establish_an_association() {
    for agent in AGENTS {
        for bytes in [b"".as_slice(), br#"{"message":"no SDK acknowledgement"}"#] {
            cannot_publish(agent, bytes);
        }
    }
}

#[test]
fn canonical_acknowledgements_decode_exact_ids_across_single_byte_chunks() -> TestResult {
    for agent in AGENTS {
        let bytes = acknowledgement(agent, r"s-\u03bb-\ud83d\ude80").into_bytes();
        let chunks: Vec<_> = bytes.chunks(1).collect();
        let observed = observe_sdk_chunks(agent, None, &chunks, true)?;
        assert_eq!(observed.session.as_deref(), Some("s-λ-🚀"));
        assert!(observed.completed, "{agent} omitted canonical completion");
        assert!(observed.peak_retained_bytes <= RETAINED_BOUND);
    }
    Ok(())
}

#[test]
fn missing_or_empty_identity_and_missing_terminal_discriminators_cannot_publish() {
    for agent in AGENTS {
        let valid = acknowledgement(agent, SESSION);
        let key = identity_key(agent);
        let without_id = valid.replace(&format!("\"{key}\":\"{SESSION}\""), "\"unrelated\":0");
        cannot_publish(agent, without_id.as_bytes());
        rejects(agent, acknowledgement(agent, "").as_bytes());
        let discriminator = match agent {
            "claude" => "\"subtype\":\"success\"",
            "codex" => "\"type\":\"turn.completed\"",
            _ => "\"status\":\"SUCCESS\"",
        };
        cannot_publish(
            agent,
            valid.replace(discriminator, "\"unrelated\":0").as_bytes(),
        );
    }
    cannot_publish(
        "claude",
        br#"{"type":"result","subtype":"success","session_id":"s"}"#,
    );
    cannot_publish(
        "agy",
        r#"{"status":"SUCCESS","conversation_id":"native-observer-session","response":"review λ","error":"provider failure"}"#.as_bytes(),
    );
    cannot_publish(
        "agy",
        r#"{"status":"SUCCESS","conversation_id":"native-observer-session","response":"review λ","error":null}"#.as_bytes(),
    );
}

#[test]
fn selected_acknowledgement_fields_require_their_declared_scalar_types() {
    for agent in AGENTS {
        let valid = acknowledgement(agent, SESSION);
        let encoded = format!("\"{}\":\"{SESSION}\"", identity_key(agent));
        for value in ["null", "42", "{}", "[]"] {
            let changed = valid.replace(&encoded, &format!("\"{}\":{value}", identity_key(agent)));
            rejects(agent, changed.as_bytes());
        }
    }
    rejects(
        "claude",
        br#"{"type":"result","subtype":"success","is_error":"false","session_id":"s"}"#,
    );
    rejects("claude", br#"{"type":false,"session_id":"s"}"#);
    rejects("agy", br#"{"status":false,"conversation_id":"s"}"#);
}

#[test]
fn nested_acknowledgement_lookalikes_cannot_spoof_top_level_identity() {
    for agent in AGENTS {
        let bytes = match agent {
            "claude" => {
                r#"{"type":"result","subtype":"success","is_error":false,"result":{"session_id":"spoof"}}"#
            }
            "codex" => {
                r#"{"type":"thread.started","item":{"thread_id":"spoof"}}
{"type":"turn.started"}
{"type":"turn.completed"}"#
            }
            _ => r#"{"status":"SUCCESS","response":{"conversation_id":"spoof"}}"#,
        };
        cannot_publish(agent, bytes.as_bytes());
        let wrapped = format!("{{\"payload\":{}}}", acknowledgement(agent, SESSION).trim());
        if agent != "codex" {
            cannot_publish(agent, wrapped.as_bytes());
        }
    }
}

#[test]
fn duplicate_selected_keys_and_conflicting_observed_ids_are_rejected() {
    for agent in AGENTS {
        let valid = acknowledgement(agent, SESSION);
        let key = identity_key(agent);
        let identity = format!("\"{key}\":\"{SESSION}\"");
        for duplicate in [SESSION, "conflicting-session"] {
            let changed =
                valid.replace(&identity, &format!("{identity},\"{key}\":\"{duplicate}\""));
            rejects(agent, changed.as_bytes());
        }
        let changed = format!("{valid}{}", acknowledgement(agent, "conflicting-session"));
        rejects(agent, changed.as_bytes());
    }
}

#[test]
fn truncated_tail_and_unclean_reader_end_reject_an_earlier_valid_acknowledgement() {
    for agent in AGENTS {
        let valid = acknowledgement(agent, SESSION);
        for tail in ["{", "{\"body\":\"unfinished", "{\"body\":1e", "[]"] {
            rejects(agent, format!("{valid}{tail}").as_bytes());
        }
        let chunks = [valid.as_bytes()];
        assert!(observe_sdk_chunks(agent, None, &chunks, false).is_err());
    }
}

#[test]
fn resume_requires_the_exact_previously_requested_session_for_every_provider() -> TestResult {
    for agent in AGENTS {
        let bytes = acknowledgement(agent, SESSION).into_bytes();
        let chunks: Vec<_> = bytes.chunks(5).collect();
        let observed = observe_sdk_chunks(agent, Some(SESSION), &chunks, true)?;
        assert_eq!(observed.session.as_deref(), Some(SESSION));
        assert!(observed.completed);
        assert!(observe_sdk_chunks(agent, Some("other-session"), &chunks, true).is_err());
    }
    Ok(())
}

#[test]
fn codex_completion_requires_canonical_thread_and_turn_order() {
    let streams = [
        r#"{"type":"turn.completed"}"#,
        r#"{"type":"thread.started","thread_id":"s"}
{"type":"turn.completed"}"#,
        r#"{"type":"turn.started"}
{"type":"thread.started","thread_id":"s"}
{"type":"turn.completed"}"#,
        r#"{"type":"thread.started","thread_id":"s"}
{"type":"thread.started","thread_id":"s"}
{"type":"turn.started"}
{"type":"turn.completed"}"#,
        r#"{"type":"thread.started","thread_id":"s"}
{"type":"turn.started"}
{"type":"turn.failed","error":{"message":"failed"}}"#,
    ];
    for bytes in streams {
        rejects("codex", bytes.as_bytes());
    }
}

#[test]
fn large_unknown_bodies_before_and_after_identity_preserve_exact_acknowledgement() -> TestResult {
    let body = "x".repeat(128 * 1024);
    for agent in AGENTS {
        let valid = acknowledgement(agent, SESSION);
        let identity = format!("\"{}\":\"{SESSION}\"", identity_key(agent));
        let expanded = valid.replace(
            &identity,
            &format!("\"body_before\":\"{body}\",{identity},\"body_after\":\"{body}\""),
        );
        let chunks: Vec<_> = expanded.as_bytes().chunks(17).collect();
        let observed = observe_sdk_chunks(agent, None, &chunks, true)?;
        assert_eq!(observed.session.as_deref(), Some(SESSION));
        assert!(observed.completed);
        assert!(observed.peak_retained_bytes <= RETAINED_BOUND);
    }
    Ok(())
}

#[test]
fn unknown_keys_are_safely_skipped_while_selected_identifiers_are_bounded() -> TestResult {
    let long_key = format!("{}session_id", "k".repeat(128 * 1024));
    for agent in AGENTS {
        let valid = acknowledgement(agent, SESSION);
        let expanded = valid.replacen('{', &format!("{{\"{long_key}\":\"spoof\","), 1);
        let chunks: Vec<_> = expanded.as_bytes().chunks(13).collect();
        let observed = observe_sdk_chunks(agent, None, &chunks, true)?;
        assert_eq!(observed.session.as_deref(), Some(SESSION));
        assert!(observed.completed);
        assert!(observed.peak_retained_bytes <= RETAINED_BOUND);
        let key_only = format!("{{\"{long_key}\":\"spoof\"}}");
        cannot_publish(agent, key_only.as_bytes());

        let boundary = "s".repeat(1024);
        let bytes = acknowledgement(agent, &boundary);
        let observed = observe_sdk_chunks(agent, None, &[bytes.as_bytes()], true)?;
        assert_eq!(observed.session.as_deref(), Some(boundary.as_str()));
        assert!(observed.completed);
        let escaped_boundary = acknowledgement(agent, &r"\u0073".repeat(1024));
        let chunks: Vec<_> = escaped_boundary.as_bytes().chunks(1).collect();
        let observed = observe_sdk_chunks(agent, None, &chunks, true)?;
        assert_eq!(observed.session.as_deref(), Some(boundary.as_str()));
        assert!(observed.completed);
        assert!(observed.peak_retained_bytes <= RETAINED_BOUND);
        rejects(agent, acknowledgement(agent, &"s".repeat(1025)).as_bytes());
        rejects(
            agent,
            acknowledgement(agent, &r"\u0073".repeat(1025)).as_bytes(),
        );
    }
    Ok(())
}

#[test]
fn nesting_and_invalid_utf8_or_escape_sequences_fail_closed_across_chunk_boundaries() -> TestResult
{
    for agent in AGENTS {
        let valid = acknowledgement(agent, SESSION);
        let nested = |depth: usize| {
            valid.replacen(
                '{',
                &format!("{{\"body\":{}0{},", "[".repeat(depth), "]".repeat(depth)),
                1,
            )
        };
        let boundary = nested(63);
        let observed = observe_sdk_chunks(agent, None, &[boundary.as_bytes()], true)?;
        assert_eq!(observed.session.as_deref(), Some(SESSION));
        assert!(observed.completed);
        assert!(observed.peak_retained_bytes <= RETAINED_BOUND);
        rejects(agent, nested(64).as_bytes());

        for malformed in [
            b"{\"body\":\"\xc3(\"}".as_slice(),
            b"{\"body\":\"\xf0\x80\x80\x80\"}".as_slice(),
            br#"{"body":"\uD800x"}"#.as_slice(),
            br#"{"body":"\uDC00"}"#.as_slice(),
            br#"{"body":"\q"}"#.as_slice(),
        ] {
            let mut bytes = valid.as_bytes().to_vec();
            bytes.extend_from_slice(malformed);
            let chunks: Vec<_> = bytes.chunks(1).collect();
            assert!(observe_sdk_chunks(agent, None, &chunks, true).is_err());
        }
    }
    Ok(())
}
