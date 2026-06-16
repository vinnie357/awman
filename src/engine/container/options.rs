//! Typed `ContainerOption` enum and surrounding option types.
//!
//! Every flag the legacy `oldsrc/runtime/{docker,apple,mod}.rs` exposes
//! becomes one variant here. Adding a new option is one variant + one branch
//! in `ResolvedContainerOptions::ingest`.

use std::path::PathBuf;

/// A reference to a container image (e.g. `awman-myproj-claude:latest`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef(pub String);

impl ImageRef {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Container entrypoint command + args (e.g. `["claude", "--print"]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entrypoint(pub Vec<String>);

impl Entrypoint {
    pub fn new(parts: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self(parts.into_iter().map(Into::into).collect())
    }
}

/// Stable name for a container (e.g. `awman-abc123`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerName(pub String);

impl ContainerName {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A directory or file overlay to mount into the container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlaySpec {
    pub host_path: PathBuf,
    pub container_path: PathBuf,
    pub permission: OverlayPermission,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayPermission {
    ReadOnly,
    ReadWrite,
}

impl OverlayPermission {
    pub fn as_str(&self) -> &'static str {
        match self {
            OverlayPermission::ReadOnly => "ro",
            OverlayPermission::ReadWrite => "rw",
        }
    }
}

/// A passthrough environment variable (read from host at launch time).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvVar(pub String);

/// A literal env-var key/value pair injected into the container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvLiteral {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum YoloMode {
    #[default]
    Disabled,
    Enabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AutoMode {
    #[default]
    Disabled,
    Enabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlanMode {
    #[default]
    Disabled,
    Enabled,
}

/// CPU limit in fractional cores (e.g. `2.0` for two cores).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CpuLimit(pub f64);

/// Memory limit in megabytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryLimit(pub u64);

/// How a model flag is delivered to the agent (e.g. `--model NAME` vs
/// `--model-claude-opus-4-6`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelFlagForm {
    /// `--model NAME`
    Argument(String),
    /// A standalone shorthand like `--model-claude-opus-4-6`.
    Shorthand(String),
}

/// A bundle of host-side agent settings prepared by `OverlayEngine`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSettings {
    /// Container `$HOME` (typically `/root` or `/home/<user>`).
    pub container_home: String,
    /// Pre-built overlay specs derived from the host's agent config files.
    pub overlays: Vec<OverlaySpec>,
}

/// Every knob a `AgentInstance` accepts. Adding a new option is a single
/// variant and a single branch in `ResolvedContainerOptions::ingest`.
#[derive(Debug, Clone, PartialEq)]
pub enum ContainerOption {
    Image(ImageRef),
    Entrypoint(Entrypoint),
    Overlay(OverlaySpec),
    EnvPassthrough(EnvVar),
    EnvLiteral(EnvLiteral),
    SeededPrompt(String),
    Interactive(bool),
    AllowDocker(bool),
    Yolo(YoloMode),
    Auto(AutoMode),
    Plan(PlanMode),
    WorkingDir(PathBuf),
    Name(ContainerName),
    Cpu(CpuLimit),
    Memory(MemoryLimit),
    AgentSettingsPassthrough(AgentSettings),
    AgentCredentials {
        env_vars: Vec<(String, String)>,
    },
    DisallowedTools(Vec<String>),
    AllowedTools(Vec<String>),
    Model {
        flag: ModelFlagForm,
    },
    NonInteractivePrintFlag(String),
    /// Container-side `$HOME` remapped from `/root` when a non-root `USER`
    /// directive is detected in the agent's Dockerfile.
    DockerfileUser(String),
    /// Session identifier — emitted as `--label awman.session=<id>` so
    /// `list_running` can attribute containers to a specific awman session.
    SessionLabel(String),
    /// Per-agent mode flags (yolo, auto, plan) — emitted as literal argv
    /// strings after the entrypoint in `build_run_argv`.
    AgentModeFlags(Vec<String>),
    /// The flag name to use when emitting disallowed tools (e.g. `--disallowedTools`).
    DisallowedToolsFlag(String),
    /// The flag name to use when emitting allowed tools (e.g. `--allowedTools`).
    AllowedToolsFlag(String),
    /// Keep the container after exit (do not pass `--rm`).
    KeepContainer,
    /// System prompt delivered via a file mount + CLI flag (e.g.
    /// `--append-system-prompt-file <container_path>`).
    SystemPromptFile {
        host_path: PathBuf,
        container_path: PathBuf,
        flag: String,
    },
    /// System prompt delivered via an env var pointing to a mounted file
    /// (e.g. `GEMINI_SYSTEM_MD=<container_path>`).
    SystemPromptEnvFile {
        env_var: String,
        host_path: PathBuf,
        container_path: PathBuf,
    },
    /// System prompt delivered as inline text via a CLI flag (e.g.
    /// `--system <text>` for cline).
    SystemPromptInline {
        flag: String,
        text: String,
    },
    /// Extra workspace dir for the agent (e.g. `--add-dir <container_path>`).
    AgentAddDir {
        flag: String,
        container_path: PathBuf,
    },
}

/// Injection-time dedup: drop any entry from `agent_credentials` whose
/// credential key maps to the same provider service as a key already declared
/// in `env_passthrough` or `env_literal`.
///
/// Mirrors the rationale of the sbx path's `CLAUDE_CODE_OAUTH_TOKEN` silent
/// skip ([`crate::engine::sandbox::dsbx::auth::inject_credentials`]):
/// when the harness has **declared** an env var (via `env(VAR)`) that already
/// authenticates the same provider, the keychain OAuth token is redundant and
/// its presence causes the container to receive two conflicting credentials for
/// the same service. Keying on declared env — never `std::env::var` — so cloud
/// harnesses that have no declared anthropic var continue to receive keychain
/// OAuth unaffected.
///
/// Example: harness declares `env(ANTHROPIC_API_KEY)` → service "anthropic";
/// keychain resolves `CLAUDE_CODE_OAUTH_TOKEN` → also service "anthropic".
/// Result: `CLAUDE_CODE_OAUTH_TOKEN` is dropped from `agent_credentials`.
pub(crate) fn dedup_credentials_by_declared_env(
    agent_credentials: &mut Vec<(String, String)>,
    env_passthrough: &[EnvVar],
    env_literal: &[EnvLiteral],
) {
    // Collect the set of provider services already covered by declared env vars.
    let covered_services: Vec<&'static str> = env_passthrough
        .iter()
        .map(|v| v.0.as_str())
        .chain(env_literal.iter().map(|l| l.key.as_str()))
        .filter_map(crate::engine::auth::service_for_credential)
        .collect();

    if covered_services.is_empty() {
        return;
    }

    // Retain only credentials whose service is NOT already covered by a
    // harness-declared env var.
    agent_credentials.retain(|(key, _)| {
        match crate::engine::auth::service_for_credential(key) {
            Some(service) => !covered_services.contains(&service),
            // Credential with no known service mapping — retain unconditionally.
            None => true,
        }
    });
}

/// Resolved option bag — all options merged into a single struct that the
/// backend consumes. Conflicting options are detected here.
#[derive(Debug, Clone, Default)]
pub struct ResolvedContainerOptions {
    pub image: Option<ImageRef>,
    pub entrypoint: Option<Entrypoint>,
    pub overlays: Vec<OverlaySpec>,
    pub env_passthrough: Vec<EnvVar>,
    pub env_literal: Vec<EnvLiteral>,
    pub seeded_prompt: Option<String>,
    pub interactive: bool,
    pub allow_docker: bool,
    pub yolo: YoloMode,
    pub auto: AutoMode,
    pub plan: PlanMode,
    pub working_dir: Option<PathBuf>,
    pub name: Option<ContainerName>,
    pub cpu: Option<CpuLimit>,
    pub memory: Option<MemoryLimit>,
    pub agent_settings: Option<AgentSettings>,
    pub agent_credentials: Vec<(String, String)>,
    pub disallowed_tools: Vec<String>,
    pub allowed_tools: Vec<String>,
    pub model: Option<ModelFlagForm>,
    pub non_interactive_flag: Option<String>,
    pub dockerfile_user: Option<String>,
    pub session_label: Option<String>,
    pub agent_mode_flags: Vec<String>,
    pub disallowed_tools_flag: Option<String>,
    pub allowed_tools_flag: Option<String>,
    pub remove_on_exit: bool,
    pub system_prompt_file: Option<(PathBuf, PathBuf, String)>,
    pub system_prompt_env_file: Option<(String, PathBuf, PathBuf)>,
    pub system_prompt_inline: Option<(String, String)>,
    pub agent_add_dirs: Vec<(String, PathBuf)>,
}

impl ResolvedContainerOptions {
    pub fn resolve(
        options: impl IntoIterator<Item = ContainerOption>,
    ) -> Result<Self, ResolveError> {
        let mut r = Self {
            yolo: YoloMode::Disabled,
            auto: AutoMode::Disabled,
            plan: PlanMode::Disabled,
            remove_on_exit: true,
            ..Self::default()
        };
        for opt in options {
            r.ingest(opt)?;
        }
        // Part A: drop agent_credentials that duplicate a service already covered
        // by a harness-declared env var.  Applies to ALL container runtimes.
        dedup_credentials_by_declared_env(
            &mut r.agent_credentials,
            &r.env_passthrough,
            &r.env_literal,
        );
        r.validate()?;
        Ok(r)
    }

    fn ingest(&mut self, opt: ContainerOption) -> Result<(), ResolveError> {
        match opt {
            ContainerOption::Image(v) => self.image = Some(v),
            ContainerOption::Entrypoint(v) => self.entrypoint = Some(v),
            ContainerOption::Overlay(v) => self.overlays.push(v),
            ContainerOption::EnvPassthrough(v) => self.env_passthrough.push(v),
            ContainerOption::EnvLiteral(v) => self.env_literal.push(v),
            ContainerOption::SeededPrompt(v) => self.seeded_prompt = Some(v),
            ContainerOption::Interactive(v) => self.interactive = v,
            ContainerOption::AllowDocker(v) => self.allow_docker = v,
            ContainerOption::Yolo(v) => self.yolo = v,
            ContainerOption::Auto(v) => self.auto = v,
            ContainerOption::Plan(v) => self.plan = v,
            ContainerOption::WorkingDir(v) => self.working_dir = Some(v),
            ContainerOption::Name(v) => self.name = Some(v),
            ContainerOption::Cpu(v) => self.cpu = Some(v),
            ContainerOption::Memory(v) => self.memory = Some(v),
            ContainerOption::AgentSettingsPassthrough(v) => self.agent_settings = Some(v),
            ContainerOption::AgentCredentials { env_vars } => {
                self.agent_credentials.extend(env_vars);
            }
            ContainerOption::DisallowedTools(v) => self.disallowed_tools.extend(v),
            ContainerOption::AllowedTools(v) => self.allowed_tools.extend(v),
            ContainerOption::Model { flag } => self.model = Some(flag),
            ContainerOption::NonInteractivePrintFlag(v) => self.non_interactive_flag = Some(v),
            ContainerOption::DockerfileUser(v) => self.dockerfile_user = Some(v),
            ContainerOption::SessionLabel(v) => self.session_label = Some(v),
            ContainerOption::AgentModeFlags(v) => self.agent_mode_flags.extend(v),
            ContainerOption::DisallowedToolsFlag(v) => self.disallowed_tools_flag = Some(v),
            ContainerOption::AllowedToolsFlag(v) => self.allowed_tools_flag = Some(v),
            ContainerOption::KeepContainer => self.remove_on_exit = false,
            ContainerOption::SystemPromptFile {
                host_path,
                container_path,
                flag,
            } => {
                self.system_prompt_file = Some((host_path, container_path, flag));
            }
            ContainerOption::SystemPromptEnvFile {
                env_var,
                host_path,
                container_path,
            } => {
                self.system_prompt_env_file = Some((env_var, host_path, container_path));
            }
            ContainerOption::SystemPromptInline { flag, text } => {
                self.system_prompt_inline = Some((flag, text));
            }
            ContainerOption::AgentAddDir {
                flag,
                container_path,
            } => {
                self.agent_add_dirs.push((flag, container_path));
            }
        }
        Ok(())
    }

    fn validate(&self) -> Result<(), ResolveError> {
        // Yolo + Plan are mutually exclusive — yolo grants permissions, plan
        // forbids them.
        if matches!(self.yolo, YoloMode::Enabled) && matches!(self.plan, PlanMode::Enabled) {
            return Err(ResolveError::Conflict(
                "yolo and plan modes are mutually exclusive".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("conflicting container options: {0}")]
    Conflict(String),
}

impl From<ResolveError> for crate::engine::error::EngineError {
    fn from(e: ResolveError) -> Self {
        match e {
            ResolveError::Conflict(msg) => {
                crate::engine::error::EngineError::ConflictingOptions(msg)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn yolo_and_plan_conflict_returns_error() {
        let result = ResolvedContainerOptions::resolve([
            ContainerOption::Yolo(YoloMode::Enabled),
            ContainerOption::Plan(PlanMode::Enabled),
        ]);
        assert!(
            matches!(result, Err(ResolveError::Conflict(_))),
            "expected Conflict, got {result:?}"
        );
    }

    #[test]
    fn all_options_round_trip_to_resolved() {
        let image = ImageRef::new("my-image:latest");
        let entrypoint = Entrypoint::new(["claude", "--print"]);
        let result = ResolvedContainerOptions::resolve([
            ContainerOption::Image(image.clone()),
            ContainerOption::Entrypoint(entrypoint.clone()),
            ContainerOption::Interactive(true),
            ContainerOption::AllowedTools(vec!["Bash".to_string()]),
            ContainerOption::Yolo(YoloMode::Disabled),
        ]);
        let resolved = result.expect("from_iter should succeed");
        assert_eq!(
            resolved.image.as_ref().map(|i| i.as_str()),
            Some("my-image:latest")
        );
        assert_eq!(
            resolved.entrypoint.as_ref().map(|e| &e.0),
            Some(&vec!["claude".to_string(), "--print".to_string()])
        );
        assert!(resolved.interactive);
        assert_eq!(resolved.allowed_tools, vec!["Bash".to_string()]);
        assert!(matches!(resolved.yolo, YoloMode::Disabled));
    }

    #[test]
    fn dedup_is_not_required_at_resolve_level() {
        let host = PathBuf::from("/host/overlay");
        let container = PathBuf::from("/container/overlay");
        let spec = OverlaySpec {
            host_path: host.clone(),
            container_path: container.clone(),
            permission: OverlayPermission::ReadOnly,
        };
        let result = ResolvedContainerOptions::resolve([
            ContainerOption::Overlay(spec.clone()),
            ContainerOption::Overlay(spec.clone()),
            ContainerOption::Overlay(spec.clone()),
        ]);
        let resolved = result.expect("from_iter should succeed");
        // Multiple overlay entries accumulate — dedup is caller's responsibility.
        assert_eq!(resolved.overlays.len(), 3);
    }

    // ── Part A: injection-time credential dedup ───────────────────────────────

    /// When the harness declares ANTHROPIC_API_KEY via env_passthrough, the
    /// keychain CLAUDE_CODE_OAUTH_TOKEN (same "anthropic" service) must be
    /// dropped from agent_credentials — only one set of anthropic creds reaches
    /// the container.
    #[test]
    fn declared_anthropic_env_passthrough_drops_oauth_token_from_agent_credentials() {
        let resolved = ResolvedContainerOptions::resolve([
            // Harness explicitly declared ANTHROPIC_API_KEY via env(VAR) overlay.
            ContainerOption::EnvPassthrough(EnvVar("ANTHROPIC_API_KEY".into())),
            // Keychain resolved the OAuth token for the same provider.
            ContainerOption::AgentCredentials {
                env_vars: vec![("CLAUDE_CODE_OAUTH_TOKEN".into(), "sk-ant-oat-secret".into())],
            },
        ])
        .expect("resolve must succeed");

        assert!(
            resolved.agent_credentials.is_empty(),
            "CLAUDE_CODE_OAUTH_TOKEN maps to service 'anthropic', same as \
             ANTHROPIC_API_KEY — it must be dropped; got: {:?}",
            resolved.agent_credentials
        );
        // The declared passthrough must be kept.
        assert!(
            resolved
                .env_passthrough
                .iter()
                .any(|v| v.0 == "ANTHROPIC_API_KEY"),
            "ANTHROPIC_API_KEY must remain in env_passthrough"
        );
    }

    /// No declared anthropic var → cloud harness path — keychain OAuth is
    /// retained (dedup must NOT fire when the declared env is absent).
    #[test]
    fn no_declared_anthropic_var_retains_oauth_token_in_agent_credentials() {
        let resolved = ResolvedContainerOptions::resolve([
            // Harness declared a non-anthropic var (e.g. OpenAI for a different agent).
            ContainerOption::EnvPassthrough(EnvVar("OPENAI_API_KEY".into())),
            ContainerOption::AgentCredentials {
                env_vars: vec![("CLAUDE_CODE_OAUTH_TOKEN".into(), "sk-ant-oat-secret".into())],
            },
        ])
        .expect("resolve must succeed");

        assert!(
            resolved
                .agent_credentials
                .iter()
                .any(|(k, _)| k == "CLAUDE_CODE_OAUTH_TOKEN"),
            "no declared anthropic env var → OAuth token must be retained; \
             got: {:?}",
            resolved.agent_credentials
        );
    }

    /// Declared via env_literal (not env_passthrough) also triggers the dedup.
    #[test]
    fn declared_anthropic_env_literal_drops_oauth_token_from_agent_credentials() {
        let resolved = ResolvedContainerOptions::resolve([
            ContainerOption::EnvLiteral(EnvLiteral {
                key: "ANTHROPIC_API_KEY".into(),
                value: "sk-ant-key-literal".into(),
            }),
            ContainerOption::AgentCredentials {
                env_vars: vec![("CLAUDE_CODE_OAUTH_TOKEN".into(), "sk-ant-oat-secret".into())],
            },
        ])
        .expect("resolve must succeed");

        assert!(
            resolved.agent_credentials.is_empty(),
            "ANTHROPIC_API_KEY via env_literal must still trigger dedup; \
             got: {:?}",
            resolved.agent_credentials
        );
    }

    /// A credential with no known service mapping (e.g. a custom env var
    /// injected via agent_credentials) is never dropped by the dedup.
    #[test]
    fn unmapped_credential_is_never_dropped_by_dedup() {
        let resolved = ResolvedContainerOptions::resolve([
            ContainerOption::EnvPassthrough(EnvVar("ANTHROPIC_API_KEY".into())),
            ContainerOption::AgentCredentials {
                env_vars: vec![
                    ("CLAUDE_CODE_OAUTH_TOKEN".into(), "sk-ant-oat".into()),
                    ("MY_CUSTOM_INTERNAL_TOKEN".into(), "custom-value".into()),
                ],
            },
        ])
        .expect("resolve must succeed");

        // OAuth token (anthropic) dropped; custom token (no mapping) retained.
        assert!(
            !resolved
                .agent_credentials
                .iter()
                .any(|(k, _)| k == "CLAUDE_CODE_OAUTH_TOKEN"),
            "OAuth token must be dropped when anthropic is declared"
        );
        assert!(
            resolved
                .agent_credentials
                .iter()
                .any(|(k, _)| k == "MY_CUSTOM_INTERNAL_TOKEN"),
            "unmapped credential must survive dedup; got: {:?}",
            resolved.agent_credentials
        );
    }

    /// When agent_credentials is empty the dedup is a no-op (no panic, no error).
    #[test]
    fn dedup_with_empty_agent_credentials_is_noop() {
        let resolved = ResolvedContainerOptions::resolve([ContainerOption::EnvPassthrough(
            EnvVar("ANTHROPIC_API_KEY".into()),
        )])
        .expect("resolve must succeed");
        assert!(resolved.agent_credentials.is_empty());
    }

    // ── Part A: dedup_credentials_by_declared_env unit tests ─────────────────

    #[test]
    fn dedup_fn_drops_oauth_when_anthropic_passthrough_declared() {
        let mut creds = vec![("CLAUDE_CODE_OAUTH_TOKEN".into(), "tok".into())];
        let pt = vec![EnvVar("ANTHROPIC_API_KEY".into())];
        dedup_credentials_by_declared_env(&mut creds, &pt, &[]);
        assert!(
            creds.is_empty(),
            "CLAUDE_CODE_OAUTH_TOKEN must be dropped; got: {creds:?}"
        );
    }

    #[test]
    fn dedup_fn_retains_oauth_when_no_anthropic_declared() {
        let mut creds = vec![("CLAUDE_CODE_OAUTH_TOKEN".into(), "tok".into())];
        let pt = vec![EnvVar("OPENAI_API_KEY".into())]; // covers openai, not anthropic
        dedup_credentials_by_declared_env(&mut creds, &pt, &[]);
        assert_eq!(
            creds.len(),
            1,
            "no anthropic declared → OAuth must be retained"
        );
    }

    #[test]
    fn dedup_fn_handles_env_literal_source() {
        let mut creds = vec![("CLAUDE_CODE_OAUTH_TOKEN".into(), "tok".into())];
        let lit = vec![EnvLiteral {
            key: "ANTHROPIC_API_KEY".into(),
            value: "literal-key".into(),
        }];
        dedup_credentials_by_declared_env(&mut creds, &[], &lit);
        assert!(
            creds.is_empty(),
            "env_literal coverage must also trigger dedup"
        );
    }

    #[test]
    fn dedup_fn_retains_credential_when_no_declared_vars() {
        let mut creds = vec![("CLAUDE_CODE_OAUTH_TOKEN".into(), "tok".into())];
        dedup_credentials_by_declared_env(&mut creds, &[], &[]);
        assert_eq!(creds.len(), 1, "no declared vars → no dedup");
    }
}
