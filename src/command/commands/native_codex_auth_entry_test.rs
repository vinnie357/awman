//! The first auth packet enters actual exec-prompt dispatch, with the main gate closed.
use super::Command;
use super::{ExecPromptCommand, ExecPromptCommandFlags, ExecPromptCommandFrontend};
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::data::fs::api_paths::ApiPaths;
use crate::data::fs::auth_paths::AuthPathResolver;
use crate::data::message::{UserMessage, UserMessageSink};
use crate::data::session::AgentName;
use crate::engine::agent::native_codex_auth_preparation_test::{
    AuthObserver, Fixture, TestResult, UserOutput, QUESTION,
};
use crate::engine::agent_runtime::AgentFrontend;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex};

struct Frontend {
    observer: Arc<AuthObserver>,
    user: Mutex<Option<Box<dyn AgentFrontend>>>,
}
impl UserMessageSink for Frontend {
    fn write_message(&mut self, message: UserMessage) {
        self.observer
            .facts
            .lock()
            .expect("facts")
            .diagnostics
            .push(message.text);
    }
    fn replay_queued(&mut self) {}
}
impl crate::command::commands::agent_setup::HasAgentFrontend for Frontend {
    fn container_frontend(&mut self) -> Box<dyn AgentFrontend> {
        self.user
            .lock()
            .expect("user")
            .take()
            .expect("single user stream")
    }
}
impl crate::command::commands::agent_setup::AgentSetupFrontend for Frontend {
    fn ask_agent_setup(
        &mut self,
        _: &AgentName,
        _: &AgentName,
        _: bool,
        _: bool,
    ) -> Result<crate::command::commands::agent_setup::AgentSetupDecision, CommandError> {
        panic!("generic agent setup reached")
    }
    fn record_fallback(&mut self, _: &AgentName, _: &AgentName) {
        panic!("generic fallback reached")
    }
}
impl crate::command::commands::agent_auth::AgentAuthFrontend for Frontend {
    fn ask_agent_auth_consent(
        &mut self,
        _: &AgentName,
        _: &[&str],
    ) -> Result<crate::command::commands::agent_auth::AgentAuthDecision, CommandError> {
        panic!("generic host credential consent reached")
    }
}
impl crate::command::commands::mount_scope::MountScopeFrontend for Frontend {
    fn ask_mount_scope(
        &mut self,
        _: &Path,
        _: &Path,
    ) -> Result<crate::command::commands::mount_scope::MountScopeDecision, CommandError> {
        panic!("auth requested review-source mount")
    }
}
impl crate::engine::acp::AcpFrontend for Frontend {
    fn render_update(&mut self, _: crate::engine::acp::SessionUpdate) {
        panic!("ACP reached")
    }
    fn request_permission(
        &mut self,
        _: crate::engine::acp::PermissionRequest,
    ) -> crate::engine::acp::PermissionDecision {
        panic!("ACP reached")
    }
    fn next_prompt(&mut self) -> Option<String> {
        panic!("auth requested model question")
    }
}
#[async_trait::async_trait]
impl ExecPromptCommandFrontend for Frontend {}

fn command(f: &Fixture) -> ExecPromptCommand {
    let observer = Arc::clone(&f.observer);
    // Named missing observer seam must fire at actual generic resolver entry.
    // auth=none in the synthetic repo prevents host lookup even on a wrong route.
    let auth = crate::engine::auth::AuthEngine::with_paths(
        AuthPathResolver::at_home(f.path().join("config")),
        ApiPaths::from_root(f.path().join("api")),
    )
    .with_agent_auth_resolution_observer(Arc::new(move || observer.event("generic-auth")));
    let engines = Engines {
        runtime: f.runtime.clone(),
        container_runtime: Some(Arc::clone(&f.runtime)),
        sandbox_runtime: None,
        git_engine: Arc::new(crate::engine::git::GitEngine::new()),
        overlay_engine: Arc::clone(&f.overlay),
        auth_engine: Arc::new(auth),
        agent_engine: Arc::new(f.engine.clone()),
        workflow_state_store: Arc::new(crate::data::EngineWorkflowStateStore::at_git_root(
            f.session.git_root(),
        )),
    };
    let mut command = ExecPromptCommand::new(
        ExecPromptCommandFlags {
            startup_gate_control: None,
            startup_gate_timeout: 120,
            prompt: Some(QUESTION.into()),
            non_interactive: false,
            plan: false,
            allow_docker: false,
            isolated_settings: false,
            yolo: false,
            auto: false,
            agent: Some("codex".into()),
            model: Some("explicit-fixture-model".into()),
            launch_mode: None,
            overlay: Vec::new(),
            issue_source: crate::engine::issue::IssueSourceFlags { issue: None },
        },
        engines,
        f.session.clone(),
    );
    // Same resolved native fields populated by from_input; no engine bypass.
    command.native_print = true;
    command
}
fn frontend(f: &Fixture) -> (Box<dyn ExecPromptCommandFrontend>, UserOutput) {
    let (user, output) = f.frontend();
    (
        Box::new(Frontend {
            observer: Arc::clone(&f.observer),
            user: Mutex::new(Some(user)),
        }),
        output,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_command_rejects_unqualified_selector_classes_before_material_or_generic_auth(
) -> TestResult {
    // Explicit ADR0002 inventory; these are synthetic unqualified selections, not real credentials.
    for selector in [
        "CODEX_HOME",
        "CODEX_SQLITE_HOME",
        "CODEX_API_KEY",
        "CODEX_ACCESS_TOKEN",
        "OPENAI_FEDERATION_RULE_ID",
        "OPENAI_IDENTITY_TOKEN_FILE",
        "OPENAI_WORKLOAD_IDENTITY_CONTEXT",
        "CODEX_REFRESH_TOKEN_URL_OVERRIDE",
        "CODEX_REVOKE_TOKEN_URL_OVERRIDE",
        "CODEX_APP_SERVER_LOGIN_CLIENT_ID",
        "OPENAI_API_KEY",
    ] {
        let f = Fixture::new(&[(selector, "synthetic-unqualified-selection")])?;
        let (front, _output) = frontend(&f);
        let result = command(&f).run_with_frontend(front).await;
        let Err(CommandError::Engine(crate::engine::error::EngineError::Config(reason))) = result
        else {
            panic!("unqualified selector must reject in native profile eligibility");
        };
        assert!(
            reason.contains(selector),
            "eligibility error must identify unsupported selector"
        );
        let facts = f.observer.facts.lock().expect("facts");
        assert!(facts.material.is_empty());
        assert!(facts.argv.is_empty());
        assert!(!facts.events.contains(&"generic-auth"));
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_command_activation_failure_is_specific_and_valid_neighbor_reaches_material(
) -> TestResult {
    let f = Fixture::new(&[])?;
    let paths = crate::data::fs::data_paths::DataPaths::from_env(f.session.env())?;
    let store = paths.root().join("native-sdk-state");
    fs::create_dir(&store)?;
    fs::set_permissions(&store, fs::Permissions::from_mode(0o777))?;
    let (front, _output) = frontend(&f);
    let result = command(&f).run_with_frontend(front).await;
    let data_error = match &result {
        Err(CommandError::Engine(crate::engine::error::EngineError::Data(error)))
        | Err(CommandError::Data(error)) => error,
        _ => panic!("unsafe exact storage root must reject with its data error"),
    };
    assert!(
        matches!(data_error, crate::data::error::DataError::Other(reason)
        if reason.contains("native SDK state") && reason.contains("unsafe directory"))
    );
    {
        let facts = f.observer.facts.lock().expect("facts");
        assert!(facts.material.is_empty());
        assert!(facts.argv.is_empty());
        assert!(!facts.events.contains(&"generic-auth"));
    }
    // Positive neighbor uses the same resolved paths; another unrelated early error cannot satisfy this test.
    fs::set_permissions(&store, fs::Permissions::from_mode(0o700))?;
    f.release_child();
    let (front, _output) = frontend(&f);
    let _main_closed = command(&f).run_with_frontend(front).await;
    assert!(!f.observer.facts.lock().expect("facts").material.is_empty());
    assert_eq!(f.observer.facts.lock().expect("facts").argv.len(), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_command_prepares_contained_auth_before_generic_material_and_never_opens_main_gate(
) -> TestResult {
    let f = Fixture::new(&[])?;
    f.release_child();
    let (front, _output) = frontend(&f);
    let result = command(&f).run_with_frontend(front).await;
    assert!(
        result.is_err(),
        "first packet must leave main production entry closed"
    );
    let state = f.observer.state_path();
    let config: toml::Value = toml::from_str(&fs::read_to_string(state.join("config.toml"))?)?;
    assert_eq!(config.as_table().expect("minimal config table").len(), 1);
    assert_eq!(config["cli_auth_credentials_store"].as_str(), Some("file"));
    f.observer.assert_unpublished();
    let facts = f.observer.facts.lock().expect("facts");
    assert!(
        !facts.material.is_empty(),
        "command bypassed strict production preparation"
    );
    assert!(!facts.events.contains(&"generic-auth"));
    assert_eq!(
        facts.argv.len(),
        1,
        "auth success must not open main execution in this packet"
    );
    let argv = &facts.argv[0];
    let guest = argv
        .windows(3)
        .position(|v| v == ["codex", "login", "--device-auth"])
        .expect("actual device-auth guest argv");
    assert_eq!(
        &argv[guest..],
        &["codex", "login", "--device-auth"],
        "auth receives no model/config/issuer override arguments"
    );
    assert_eq!(
        facts.seeded_inputs,
        vec![None],
        "actual spawn stdin seed must contain no model question"
    );
    assert!(!argv.iter().any(|arg| arg.contains(QUESTION)));
    assert!(argv
        .iter()
        .any(|arg| arg == "CODEX_HOME=/data/awman-native-sdk/codex"));
    let mounts = argv
        .windows(2)
        .filter(|p| p[0] == "-v" || p[0] == "--volume")
        .map(|p| p[1].as_str())
        .collect::<Vec<_>>();
    let expected_source = state.to_string_lossy();
    let sdk_mounts = mounts
        .iter()
        .filter(|mount| mount.split(':').nth(1) == Some("/data/awman-native-sdk/codex"))
        .collect::<Vec<_>>();
    assert_eq!(sdk_mounts.len(), 1);
    let sdk = sdk_mounts[0].split(':').collect::<Vec<_>>();
    assert_eq!(sdk[0], expected_source.as_ref());
    assert!(
        sdk.len() == 2 || (sdk.len() == 3 && sdk[2] == "rw"),
        "actual SDK mount must be exactly RW, without suffix redirection"
    );
    for mount in mounts {
        let fields = mount.split(':').collect::<Vec<_>>();
        let destination = fields.get(1).expect("mount destination");
        assert!(
            *destination == "/data/awman-native-sdk/codex"
                || destination.starts_with("/.awman/startup-gate/"),
            "auth broadened mounts beyond SDK and existing protected controls"
        );
        assert!(!mount.contains("/.codex"));
    }
    assert_eq!(facts.requests.len(), 1);
    let bindings = facts.requests[0]["bindings"]
        .as_array()
        .expect("READY request bindings");
    assert_eq!(bindings.len(), 1);
    assert_eq!(
        bindings[0]["workspace_path"],
        "/data/awman-native-sdk/codex"
    );
    assert_eq!(bindings[0]["access"], "read-write");
    assert!(!facts
        .diagnostics
        .iter()
        .any(|message| message.contains("Resolving agent credentials")
            || message.contains("Checking agent availability")));
    Ok(())
}
