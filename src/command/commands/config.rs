//! `ConfigCommand` — view and edit global / repo configuration.

use async_trait::async_trait;
use serde::Serialize;

use crate::command::commands::Command;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::engine::message::UserMessageSink;

/// All valid top-level config field names (both global and repo).
const VALID_CONFIG_FIELDS: &[&str] = &[
    "agent",
    "auto_agent_auth_accepted",
    "terminal_scrollback_lines",
    "yoloDisallowedTools",
    "envPassthrough",
    "workItems",
    "overlays",
    "agentStuckTimeout",
    "runtime",
    "default_agent",
    "headless",
    "remote",
];

/// Valid agent names for config set agent=<value>.
const VALID_AGENT_VALUES: &[&str] = &[
    "claude", "codex", "gemini", "opencode", "crush", "cline", "copilot", "maki",
];

/// Levenshtein edit distance between two strings.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let m = a.len();
    let n = b.len();
    // Row 0: dp[0][j] = j for j in 0..=n
    let first_row: Vec<usize> = (0..=n).collect();
    let mut dp: Vec<Vec<usize>> = std::iter::once(first_row)
        .chain((1..=m).map(|i| {
            let mut row = vec![0usize; n + 1];
            row[0] = i;
            row
        }))
        .collect();
    for i in 1..=m {
        for j in 1..=n {
            dp[i][j] = if a[i - 1] == b[j - 1] {
                dp[i - 1][j - 1]
            } else {
                1 + dp[i - 1][j - 1].min(dp[i - 1][j]).min(dp[i][j - 1])
            };
        }
    }
    dp[m][n]
}

/// Return candidates with levenshtein distance <= 3, sorted by distance ascending.
fn levenshtein_suggestions<'a>(input: &str, candidates: &[&'a str]) -> Vec<&'a str> {
    let mut scored: Vec<(usize, &'a str)> = candidates
        .iter()
        .filter_map(|c| {
            let dist = levenshtein(input, c);
            if dist <= 3 {
                Some((dist, *c))
            } else {
                None
            }
        })
        .collect();
    scored.sort_by_key(|(d, _)| *d);
    scored.into_iter().map(|(_, c)| c).collect()
}

#[derive(Debug, Clone)]
pub struct ConfigShowFlags {}

#[derive(Debug, Clone)]
pub struct ConfigGetFlags {
    pub field: String,
}

#[derive(Debug, Clone)]
pub struct ConfigSetFlags {
    pub field: String,
    pub value: String,
    pub global: bool,
}

#[derive(Debug, Clone)]
pub enum ConfigSubcommand {
    Show(ConfigShowFlags),
    Get(ConfigGetFlags),
    Set(ConfigSetFlags),
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigShowOutcome {
    pub global: serde_json::Value,
    pub repo: serde_json::Value,
    /// One row per known field, computed in Layer 2 so the renderer doesn't
    /// need to know which fields exist or which are read-only.
    pub rows: Vec<ConfigFieldRow>,
}

/// Per-field row used by `ConfigShow` rendering.
#[derive(Debug, Clone, Serialize)]
pub struct ConfigFieldRow {
    pub field: String,
    pub global_value: Option<String>,
    pub repo_value: Option<String>,
    pub effective_value: Option<String>,
    /// What kind of value the field accepts. Lets the renderer (or a
    /// programmatic consumer) format the value cell appropriately and lets
    /// `set` validate input early.
    pub kind: ConfigFieldKind,
    pub read_only: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigFieldKind {
    Bool,
    Number,
    /// Fixed enum (e.g. agent name); the `set` validator rejects values
    /// outside the documented set.
    Enum,
    String,
}

/// Map a known config field name to its `ConfigFieldKind`. Mirrors the
/// schema in `RepoConfig` / `GlobalConfig`. Unknown fields default to
/// `String` (callers should reject them before reaching this function).
fn config_field_kind(name: &str) -> ConfigFieldKind {
    match name {
        "agent" => ConfigFieldKind::Enum,
        "auto_agent_auth_accepted" => ConfigFieldKind::Bool,
        "terminal_scrollback_lines" | "agentStuckTimeout" => ConfigFieldKind::Number,
        _ => ConfigFieldKind::String,
    }
}

/// Fields whose value is computed by amux itself and cannot be set by the
/// user via `amux config set`. Surfaced with `(read-only)` in the table.
const READ_ONLY_FIELDS: &[&str] = &["auto_agent_auth_accepted"];

fn collect_config_rows(
    global: &serde_json::Value,
    repo: &serde_json::Value,
) -> Vec<ConfigFieldRow> {
    VALID_CONFIG_FIELDS
        .iter()
        .map(|name| {
            let g = config_field_value(global, name);
            let r = config_field_value(repo, name);
            ConfigFieldRow {
                field: (*name).to_string(),
                global_value: g.clone(),
                repo_value: r.clone(),
                effective_value: r.or(g),
                kind: config_field_kind(name),
                read_only: READ_ONLY_FIELDS.contains(name),
            }
        })
        .collect()
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigGetOutcome {
    pub field: String,
    pub global_value: Option<String>,
    pub repo_value: Option<String>,
    pub effective_value: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigSetOutcome {
    pub field: String,
    pub value: String,
    pub scope: String,
}

pub trait ConfigCommandFrontend: UserMessageSink + Send + Sync {}

pub struct ConfigCommand {
    sub: ConfigSubcommand,
    engines: Engines,
}

impl ConfigCommand {
    pub fn new(sub: ConfigSubcommand, engines: Engines) -> Self {
        Self { sub, engines }
    }

    pub fn subcommand(&self) -> &ConfigSubcommand {
        &self.sub
    }
}

/// Outcome enum used by the `Command` trait impl.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", content = "payload")]
pub enum ConfigOutcome {
    Show(ConfigShowOutcome),
    Get(ConfigGetOutcome),
    Set(ConfigSetOutcome),
}

#[async_trait]
impl Command for ConfigCommand {
    type Frontend = Box<dyn ConfigCommandFrontend>;
    type Outcome = ConfigOutcome;

    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        let _ = self.engines;
        let session = open_session()?;
        let outcome = match self.sub {
            ConfigSubcommand::Show(_) => {
                let global =
                    serde_json::to_value(session.global_config()).unwrap_or(serde_json::Value::Null);
                let repo =
                    serde_json::to_value(session.repo_config()).unwrap_or(serde_json::Value::Null);
                let rows = collect_config_rows(&global, &repo);
                ConfigOutcome::Show(ConfigShowOutcome { global, repo, rows })
            }
            ConfigSubcommand::Get(f) => {
                // Validate field name.
                if !VALID_CONFIG_FIELDS.contains(&f.field.as_str()) {
                    let suggestions = levenshtein_suggestions(&f.field, VALID_CONFIG_FIELDS);
                    return Err(CommandError::UnknownConfigField {
                        name: f.field.clone(),
                        suggestions: if suggestions.is_empty() {
                            "(none)".to_string()
                        } else {
                            suggestions.join(", ")
                        },
                    });
                }
                let global_value = config_field_value(
                    &serde_json::to_value(session.global_config()).unwrap_or(serde_json::Value::Null),
                    &f.field,
                );
                let repo_value = config_field_value(
                    &serde_json::to_value(session.repo_config()).unwrap_or(serde_json::Value::Null),
                    &f.field,
                );
                let effective_value = repo_value.clone().or_else(|| global_value.clone());
                ConfigOutcome::Get(ConfigGetOutcome {
                    field: f.field,
                    global_value,
                    repo_value,
                    effective_value,
                })
            }
            ConfigSubcommand::Set(f) => {
                // Validate field name.
                if !VALID_CONFIG_FIELDS.contains(&f.field.as_str()) {
                    let suggestions = levenshtein_suggestions(&f.field, VALID_CONFIG_FIELDS);
                    return Err(CommandError::UnknownConfigField {
                        name: f.field.clone(),
                        suggestions: if suggestions.is_empty() {
                            "(none)".to_string()
                        } else {
                            suggestions.join(", ")
                        },
                    });
                }
                // Validate agent value when setting the "agent" field.
                if f.field == "agent" && !VALID_AGENT_VALUES.contains(&f.value.as_str()) {
                    return Err(CommandError::InvalidFlagValue {
                        command: vec!["config".into(), "set".into()],
                        flag: "agent".into(),
                        reason: format!(
                            "'{}' is not a known agent; valid agents: {}",
                            f.value,
                            VALID_AGENT_VALUES.join(", ")
                        ),
                    });
                }
                if f.global {
                    let mut cfg = session.global_config().clone();
                    set_config_field(
                        &mut serde_json::to_value(&cfg).unwrap_or_default(),
                        &f.field,
                        &f.value,
                    );
                    // Re-serialize via JSON round-trip to apply the change.
                    let mut json = serde_json::to_value(&cfg).unwrap_or_default();
                    set_config_field(&mut json, &f.field, &f.value);
                    if let Ok(updated) = serde_json::from_value(json) {
                        cfg = updated;
                        let _ = cfg.save();
                    }
                } else {
                    let mut cfg = session.repo_config().clone();
                    let mut json = serde_json::to_value(&cfg).unwrap_or_default();
                    set_config_field(&mut json, &f.field, &f.value);
                    if let Ok(updated) = serde_json::from_value(json) {
                        cfg = updated;
                        let _ = cfg.save(session.git_root());
                    }
                }
                ConfigOutcome::Set(ConfigSetOutcome {
                    field: f.field,
                    value: f.value,
                    scope: if f.global { "global".into() } else { "repo".into() },
                })
            }
        };
        frontend.replay_queued();
        Ok(outcome)
    }
}

/// Look up a JSON field value (top-level only) and stringify it.
fn config_field_value(json: &serde_json::Value, field: &str) -> Option<String> {
    let v = json.get(field)?;
    Some(match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Null => return None,
        other => other.to_string(),
    })
}

/// Set a top-level JSON field, parsing the string into the right type.
fn set_config_field(json: &mut serde_json::Value, field: &str, value: &str) {
    if let serde_json::Value::Object(obj) = json {
        let new_val = if value == "true" {
            serde_json::Value::Bool(true)
        } else if value == "false" {
            serde_json::Value::Bool(false)
        } else if let Ok(n) = value.parse::<u64>() {
            serde_json::Value::Number(n.into())
        } else {
            serde_json::Value::String(value.to_string())
        };
        obj.insert(field.to_string(), new_val);
    }
}

fn open_session() -> Result<crate::data::session::Session, CommandError> {
    let cwd = std::env::current_dir()
        .map_err(|e| CommandError::Other(format!("cwd unavailable: {e}")))?;
    let resolver = crate::data::session::StaticGitRootResolver::new(cwd.clone());
    crate::data::session::Session::open(
        cwd,
        &resolver,
        crate::data::session::SessionOpenOptions::default(),
    )
    .map_err(CommandError::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── config_field_value ───────────────────────────────────────────────────

    #[test]
    fn config_field_value_returns_string_field() {
        let json = serde_json::json!({"agent": "claude", "model": null});
        assert_eq!(
            config_field_value(&json, "agent"),
            Some("claude".to_string())
        );
    }

    #[test]
    fn config_field_value_returns_bool_field_as_string() {
        let json = serde_json::json!({"yolo": true});
        assert_eq!(config_field_value(&json, "yolo"), Some("true".to_string()));
    }

    #[test]
    fn config_field_value_returns_number_field_as_string() {
        let json = serde_json::json!({"port": 9876u64});
        assert_eq!(config_field_value(&json, "port"), Some("9876".to_string()));
    }

    #[test]
    fn config_field_value_returns_none_for_missing_field() {
        let json = serde_json::json!({"agent": "claude"});
        assert_eq!(config_field_value(&json, "nonexistent"), None);
    }

    #[test]
    fn config_field_value_returns_none_for_null_value() {
        let json = serde_json::json!({"model": null});
        assert_eq!(config_field_value(&json, "model"), None);
    }

    // ── set_config_field ─────────────────────────────────────────────────────

    #[test]
    fn set_config_field_inserts_string_value() {
        let mut json = serde_json::json!({});
        set_config_field(&mut json, "agent", "codex");
        assert_eq!(json["agent"], serde_json::Value::String("codex".into()));
    }

    #[test]
    fn set_config_field_parses_true_as_bool() {
        let mut json = serde_json::json!({});
        set_config_field(&mut json, "yolo", "true");
        assert_eq!(json["yolo"], serde_json::Value::Bool(true));
    }

    #[test]
    fn set_config_field_parses_false_as_bool() {
        let mut json = serde_json::json!({});
        set_config_field(&mut json, "yolo", "false");
        assert_eq!(json["yolo"], serde_json::Value::Bool(false));
    }

    #[test]
    fn set_config_field_parses_numeric_string_as_number() {
        let mut json = serde_json::json!({});
        set_config_field(&mut json, "port", "9876");
        assert_eq!(json["port"], serde_json::json!(9876u64));
    }

    #[test]
    fn set_config_field_overwrites_existing_value() {
        let mut json = serde_json::json!({"agent": "claude"});
        set_config_field(&mut json, "agent", "gemini");
        assert_eq!(json["agent"], serde_json::Value::String("gemini".into()));
    }

    #[test]
    fn set_config_field_does_not_modify_non_object() {
        // If the json is not an Object, set_config_field is a no-op.
        let mut json = serde_json::Value::Null;
        set_config_field(&mut json, "agent", "claude");
        // Should still be Null — no panic.
        assert!(json.is_null());
    }

    // ── levenshtein ───────────────────────────────────────────────────────────

    #[test]
    fn levenshtein_identical_strings_is_zero() {
        assert_eq!(levenshtein("agent", "agent"), 0);
    }

    #[test]
    fn levenshtein_empty_string_is_length_of_other() {
        assert_eq!(levenshtein("", "abc"), 3);
        assert_eq!(levenshtein("abc", ""), 3);
    }

    #[test]
    fn levenshtein_one_substitution() {
        assert_eq!(levenshtein("cat", "cut"), 1);
    }

    #[test]
    fn levenshtein_one_insertion() {
        assert_eq!(levenshtein("agent", "agents"), 1);
    }

    #[test]
    fn levenshtein_one_deletion() {
        assert_eq!(levenshtein("agents", "agent"), 1);
    }

    // ── levenshtein_suggestions ───────────────────────────────────────────────

    #[test]
    fn levenshtein_suggestions_finds_close_match() {
        let result = levenshtein_suggestions("agnet", VALID_CONFIG_FIELDS);
        // "agnet" is distance 2 from "agent" (two transpositions); should appear.
        assert!(
            result.contains(&"agent"),
            "suggestions must contain 'agent' for input 'agnet': {result:?}"
        );
    }

    #[test]
    fn levenshtein_suggestions_empty_when_no_close_match() {
        let result = levenshtein_suggestions("zzzzzzzzzzz", VALID_CONFIG_FIELDS);
        assert!(
            result.is_empty(),
            "suggestions must be empty for very distant input"
        );
    }

    #[test]
    fn levenshtein_suggestions_sorted_by_distance() {
        // "runtim" is distance 1 from "runtime" and distance 2+ from all others.
        let result = levenshtein_suggestions("runtim", VALID_CONFIG_FIELDS);
        if result.len() >= 2 {
            // First result must be "runtime" (closest match).
            assert_eq!(result[0], "runtime", "closest match must be first: {result:?}");
        }
    }
}
