//! Workflow file definitions and parsing — Layer 0.
//!
//! Defines the canonical `Workflow` and `WorkflowStep` data types and supports
//! parsing from TOML and YAML files. Parsing produces serializable
//! data only — no engine logic, no DAG validation (see `workflow_dag.rs`),
//! no execution state (see `workflow_state.rs`).

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::data::error::DataError;

/// Supported workflow file formats, detected by file extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowFormat {
    Toml,
    Yaml,
}

/// Detect the workflow format from a file extension. `.md` files return
/// `MarkdownNoLongerSupported`; `.json` is explicitly rejected.
pub fn detect_format(path: &Path) -> Result<WorkflowFormat, DataError> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("md") => Err(DataError::MarkdownNoLongerSupported {
            path: path.to_path_buf(),
        }),
        Some("toml") => Ok(WorkflowFormat::Toml),
        Some("yml") | Some("yaml") => Ok(WorkflowFormat::Yaml),
        Some(other) => Err(DataError::WorkflowState(format!(
            "unsupported workflow format '.{other}': expected .toml, .yml, or .yaml"
        ))),
        None => Err(DataError::WorkflowState(
            "workflow file has no extension; expected .toml, .yml, or .yaml".into(),
        )),
    }
}

/// A single step in a multi-agent workflow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowStep {
    pub name: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    pub prompt_template: String,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
}

/// A setup phase step — executed before the main workflow steps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SetupStep {
    CloneRepo {
        url: String,
        #[serde(default)]
        branch: Option<String>,
        #[serde(default)]
        into: Option<String>,
    },
    CheckoutCreateBranch {
        branch: String,
        #[serde(default)]
        base: Option<String>,
    },
    PullBranch {
        #[serde(default)]
        remote: Option<String>,
        #[serde(default)]
        branch: Option<String>,
    },
    RunShell {
        command: String,
        #[serde(default)]
        env: Option<HashMap<String, String>>,
    },
    RunScript {
        path: String,
        #[serde(default)]
        env: Option<HashMap<String, String>>,
    },
}

/// A teardown phase step — executed after the main workflow steps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TeardownStep {
    RunShell {
        command: String,
        #[serde(default)]
        env: Option<HashMap<String, String>>,
    },
    RunScript {
        path: String,
    },
    CommitChanges {
        message: String,
        #[serde(default)]
        add_all: bool,
    },
    CreatePullRequest {
        title: String,
        #[serde(default)]
        body: Option<String>,
        #[serde(default)]
        base: Option<String>,
    },
    PushBranch {
        #[serde(default)]
        remote: Option<String>,
        #[serde(default)]
        branch: Option<String>,
    },
}

/// Parsed, validated workflow definition. The DAG (`workflow_dag.rs`) and
/// runtime state (`workflow_state.rs`) live in separate modules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workflow {
    pub title: Option<String>,
    pub steps: Vec<WorkflowStep>,
    /// Optional workflow-level default agent (overridden by step-level `agent`).
    #[serde(default)]
    pub agent: Option<String>,
    /// Optional workflow-level default model (overridden by step-level `model`).
    #[serde(default)]
    pub model: Option<String>,
    /// Setup steps run before the first workflow step.
    #[serde(default)]
    pub setup: Vec<SetupStep>,
    /// Teardown steps run after the last workflow step (or on failure, if configured).
    #[serde(default)]
    pub teardown: Vec<TeardownStep>,
    /// If true, teardown runs even when the workflow fails.
    #[serde(default)]
    pub teardown_on_failure: bool,
}

impl Workflow {
    /// Parse a workflow file's *content* given the resolved format.
    pub fn parse(content: &str, format: WorkflowFormat) -> Result<Self, DataError> {
        match format {
            WorkflowFormat::Toml => parse_toml(content),
            WorkflowFormat::Yaml => parse_yaml(content),
        }
    }

    /// Convenience: read and parse a workflow from disk.
    pub fn load(path: &Path) -> Result<Self, DataError> {
        let format = detect_format(path)?;
        let content = std::fs::read_to_string(path).map_err(|e| DataError::io(path, e))?;
        Self::parse(&content, format)
    }
}

// ─── TOML/YAML parsers ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStep {
    name: Option<String>,
    prompt: Option<String>,
    #[serde(default)]
    depends_on: Vec<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TomlWorkflow {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(rename = "step", alias = "steps", default)]
    steps: Vec<RawStep>,
    #[serde(default)]
    setup: Vec<SetupStep>,
    #[serde(default)]
    teardown: Vec<TeardownStep>,
    #[serde(default)]
    teardown_on_failure: bool,
}

#[derive(Debug, Deserialize)]
struct YamlWorkflow {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    steps: Vec<RawStep>,
    #[serde(default)]
    setup: Vec<SetupStep>,
    #[serde(default)]
    teardown: Vec<TeardownStep>,
    #[serde(default)]
    teardown_on_failure: bool,
}

fn strip_bom(s: &str) -> &str {
    s.strip_prefix('\u{FEFF}').unwrap_or(s)
}

fn raw_to_steps(raw: Vec<RawStep>) -> Result<Vec<WorkflowStep>, DataError> {
    let mut steps = Vec::with_capacity(raw.len());
    for (idx, r) in raw.into_iter().enumerate() {
        let name = r.name.ok_or_else(|| {
            DataError::WorkflowState(format!("step {idx}: missing required field 'name'"))
        })?;
        let prompt_raw = r.prompt.ok_or_else(|| {
            DataError::WorkflowState(format!("step {idx} ('{name}'): missing required 'prompt'"))
        })?;
        let prompt_template = prompt_raw.trim().to_string();
        steps.push(WorkflowStep {
            name,
            depends_on: r.depends_on,
            prompt_template,
            agent: r.agent,
            model: r.model,
        });
    }
    if steps.is_empty() {
        return Err(DataError::WorkflowState(
            "workflow file contains no steps".into(),
        ));
    }
    Ok(steps)
}

fn parse_toml(content: &str) -> Result<Workflow, DataError> {
    let stripped = strip_bom(content);
    let parsed: TomlWorkflow =
        toml::from_str(stripped).map_err(|e| DataError::WorkflowState(format!("toml: {e}")))?;
    let title = parsed.title.or(parsed.name);
    Ok(Workflow {
        title,
        agent: parsed.agent,
        model: parsed.model,
        steps: raw_to_steps(parsed.steps)?,
        setup: parsed.setup,
        teardown: parsed.teardown,
        teardown_on_failure: parsed.teardown_on_failure,
    })
}

fn parse_yaml(content: &str) -> Result<Workflow, DataError> {
    let stripped = strip_bom(content);
    let parsed: YamlWorkflow = serde_yaml::from_str(stripped)
        .map_err(|e| DataError::WorkflowState(format!("yaml: {e}")))?;
    let title = parsed.title.or(parsed.name);
    Ok(Workflow {
        title,
        agent: parsed.agent,
        model: parsed.model,
        steps: raw_to_steps(parsed.steps)?,
        setup: parsed.setup,
        teardown: parsed.teardown,
        teardown_on_failure: parsed.teardown_on_failure,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn detect_format_toml_yaml() {
        assert_eq!(
            detect_format(&PathBuf::from("a.toml")).unwrap(),
            WorkflowFormat::Toml
        );
        assert_eq!(
            detect_format(&PathBuf::from("a.yml")).unwrap(),
            WorkflowFormat::Yaml
        );
        assert_eq!(
            detect_format(&PathBuf::from("a.yaml")).unwrap(),
            WorkflowFormat::Yaml
        );
        assert!(detect_format(&PathBuf::from("a.json")).is_err());
        assert!(detect_format(&PathBuf::from("noext")).is_err());
    }

    #[test]
    fn detect_format_md_returns_markdown_no_longer_supported() {
        let err = detect_format(&PathBuf::from("a.md")).unwrap_err();
        assert!(
            matches!(err, DataError::MarkdownNoLongerSupported { .. }),
            "expected MarkdownNoLongerSupported, got: {err:?}"
        );
    }

    #[test]
    fn parse_toml_array_of_step() {
        let toml = r#"
title = "T"
[[step]]
name = "a"
prompt = "do A"

[[step]]
name = "b"
prompt = "do B"
depends_on = ["a"]
"#;
        let wf = Workflow::parse(toml, WorkflowFormat::Toml).unwrap();
        assert_eq!(wf.steps.len(), 2);
        assert_eq!(wf.steps[1].depends_on, vec!["a".to_string()]);
        assert!(wf.setup.is_empty());
        assert!(wf.teardown.is_empty());
        assert!(!wf.teardown_on_failure);
    }

    #[test]
    fn parse_yaml_steps() {
        let yaml = "title: Hi\nsteps:\n  - name: a\n    prompt: do A\n";
        let wf = Workflow::parse(yaml, WorkflowFormat::Yaml).unwrap();
        assert_eq!(wf.steps.len(), 1);
        assert_eq!(wf.steps[0].name, "a");
        assert!(wf.setup.is_empty());
        assert!(wf.teardown.is_empty());
    }

    #[test]
    fn parse_toml_with_setup_and_teardown() {
        let toml = r#"
name = "implement-feature"
teardown_on_failure = true

[[setup]]
type = "checkout_create_branch"
branch = "feature/my-thing"

[[setup]]
type = "run_shell"
command = "cargo fetch"

[[steps]]
name = "implement"
prompt = "Implement the feature described in SPEC.md"

[[teardown]]
type = "run_shell"
command = "cargo test"

[[teardown]]
type = "create_pull_request"
title = "feat: implement my feature"
body = "Automated PR from awman workflow"
"#;
        let wf = Workflow::parse(toml, WorkflowFormat::Toml).unwrap();
        assert_eq!(wf.title.as_deref(), Some("implement-feature"));
        assert_eq!(wf.setup.len(), 2);
        assert_eq!(wf.teardown.len(), 2);
        assert!(wf.teardown_on_failure);

        assert!(matches!(
            &wf.setup[0],
            SetupStep::CheckoutCreateBranch { branch, .. } if branch == "feature/my-thing"
        ));
        assert!(matches!(
            &wf.setup[1],
            SetupStep::RunShell { command, .. } if command == "cargo fetch"
        ));
        assert!(matches!(
            &wf.teardown[0],
            TeardownStep::RunShell { command, .. } if command == "cargo test"
        ));
        assert!(matches!(
            &wf.teardown[1],
            TeardownStep::CreatePullRequest { title, body, .. }
                if title == "feat: implement my feature" && body.as_deref() == Some("Automated PR from awman workflow")
        ));
    }

    #[test]
    fn parse_yaml_with_setup_and_teardown() {
        let yaml = r#"
name: implement-feature
teardown_on_failure: true
setup:
  - type: checkout_create_branch
    branch: feature/my-thing
  - type: run_shell
    command: cargo fetch
steps:
  - name: implement
    prompt: Implement the feature
teardown:
  - type: commit_changes
    message: "auto: implemented feature"
    add_all: true
  - type: push_branch
"#;
        let wf = Workflow::parse(yaml, WorkflowFormat::Yaml).unwrap();
        assert_eq!(wf.setup.len(), 2);
        assert_eq!(wf.teardown.len(), 2);
        assert!(wf.teardown_on_failure);

        assert!(matches!(
            &wf.teardown[0],
            TeardownStep::CommitChanges { message, add_all } if message == "auto: implemented feature" && *add_all
        ));
        assert!(matches!(
            &wf.teardown[1],
            TeardownStep::PushBranch { remote, branch } if remote.is_none() && branch.is_none()
        ));
    }

    #[test]
    fn existing_workflow_without_setup_teardown_parses() {
        let toml = r#"
[[step]]
name = "a"
prompt = "do A"
"#;
        let wf = Workflow::parse(toml, WorkflowFormat::Toml).unwrap();
        assert!(wf.setup.is_empty());
        assert!(wf.teardown.is_empty());
        assert!(!wf.teardown_on_failure);
    }
}
