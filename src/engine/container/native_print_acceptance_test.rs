//! Frozen native-print contract. Fake only the container CLI boundary.
use super::options::{ContainerName, ImageRef, OverlayPermission, ResolvedContainerOptions};
use super::process::{ContainerCli, ContainerInstance};
use super::ContainerRuntime;
use crate::data::config::env::EnvSnapshot;
use crate::data::fs::auth_paths::AuthPathResolver;
use crate::data::message::{UserMessage, UserMessageSink};
use crate::data::session::{AgentName, Session, SessionOpenOptions, StaticGitRootResolver};
use crate::engine::agent::{AgentEngine, AgentRunOptions};
use crate::engine::agent_runtime::{
    AgentFrontend, AgentInstance, AgentIo, AgentProgress, AgentStatus,
};
use crate::engine::overlay::{DirectorySpec, OverlayEngine};
use async_trait::async_trait;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const PROMPT: &str = "review 'quoted' input\n--model fake\nUnicode: λ\n";
const MODEL: &str = "native-model";
const IMAGE: &str = "native-contract-image:latest";
const FINAL: &str = "/workspace/awman-native-last-message.txt";

type Result<T> = std::result::Result<T, Box<dyn Error>>;

struct Fixture {
    _root: tempfile::TempDir,
    project: PathBuf,
    home: PathBuf,
    input: PathBuf,
    engine: AgentEngine,
    session: Session,
}

impl Fixture {
    fn new() -> Result<Self> {
        let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize()?)?;
        let project = root.path().join("project");
        let home = root.path().join("host-settings");
        let input = root.path().join("input");
        for path in [&project, &home, &input] {
            std::fs::create_dir_all(path)?;
        }
        std::fs::write(input.join("review.txt"), b"readable input\n")?;
        for (dir, file, content) in [
            (".claude", "settings.json", "{}"),
            (".codex", "config.toml", "model = \"host-model\"\n"),
            (".gemini/antigravity-cli", "settings.json", "{}"),
        ] {
            std::fs::create_dir_all(home.join(dir))?;
            std::fs::write(home.join(dir).join(file), content)?;
        }
        let resolver = StaticGitRootResolver::new(&project);
        let session = Session::open(
            project.clone(),
            &resolver,
            SessionOpenOptions {
                env: Some(EnvSnapshot::with_overrides([
                    (
                        "AWMAN_CONFIG_HOME".to_string(),
                        home.to_string_lossy().into_owned(),
                    ),
                    (
                        "AWMAN_API_ROOT".to_string(),
                        root.path().join("api").to_string_lossy().into_owned(),
                    ),
                ])),
                ..Default::default()
            },
        )?;
        let overlay = OverlayEngine::with_auth_resolver(AuthPathResolver::at_home(&home))
            .with_secret_files_provider(Arc::new(|_| Vec::new()))
            .with_credential_provider(Arc::new(|_| None));
        let engine = AgentEngine::new(
            Arc::new(overlay),
            Arc::new(ContainerRuntime::docker().with_image_home_dir_for_test("/root")),
        );
        Ok(Self {
            _root: root,
            project,
            home,
            input,
            engine,
            session,
        })
    }

    fn run_options(&self, session: Option<&str>) -> AgentRunOptions {
        AgentRunOptions {
            native_print: true,
            native_session: session.map(str::to_string),
            // Native print must force isolated settings and noninteractive I/O.
            isolated_settings: false,
            non_interactive: false,
            initial_prompt: Some(PROMPT.to_string()),
            model: Some(MODEL.to_string()),
            image_tag_override: Some(IMAGE.to_string()),
            directory_overlays: vec![DirectorySpec {
                host: self.input.to_string_lossy().into_owned(),
                container: "/workspace/input-1".to_string(),
                permission: OverlayPermission::ReadOnly,
            }],
            ..Default::default()
        }
    }

    fn resolve(&self, agent: &str, run: &AgentRunOptions) -> Result<ResolvedContainerOptions> {
        Ok(ResolvedContainerOptions::resolve(
            self.engine
                .build_options(&self.session, &AgentName::new(agent)?, run)?,
        )?)
    }
}

fn expected(agent: &str, session: Option<&str>) -> Vec<String> {
    let mut argv: Vec<String> = if agent == "codex" {
        [
            "codex",
            "exec",
            "--model",
            MODEL,
            "--json",
            "--output-last-message",
            FINAL,
            "--skip-git-repo-check",
        ]
        .into_iter()
        .map(str::to_string)
        .collect()
    } else {
        [
            agent,
            "-p",
            PROMPT,
            "--model",
            MODEL,
            "--output-format",
            "json",
        ]
        .into_iter()
        .map(str::to_string)
        .collect()
    };
    if let Some(id) = session {
        argv.extend([
            match agent {
                "claude" => "--resume",
                "agy" => "--conversation",
                _ => "resume",
            }
            .to_string(),
            id.to_string(),
        ]);
    }
    if agent == "codex" {
        argv.push("-".to_string());
    }
    argv
}

fn provider_argv(options: &ResolvedContainerOptions) -> Result<Vec<String>> {
    let argv = super::docker::build_run_argv(
        &ContainerName("native-test".into()),
        &ImageRef(IMAGE.into()),
        options,
    );
    let image = argv
        .iter()
        .position(|part| part == IMAGE)
        .ok_or("image absent from actual runtime argv")?;
    Ok(argv[image + 1..].to_vec())
}

#[test]
fn native_first_and_continuation_argv_are_canonical_after_runtime_serialization() -> Result<()> {
    let fixture = Fixture::new()?;
    for agent in ["claude", "codex", "agy"] {
        for session in [None, Some("provider-session-1")] {
            let options = fixture.resolve(agent, &fixture.run_options(session))?;
            assert_eq!(
                provider_argv(&options)?,
                expected(agent, session),
                "{agent} {session:?}"
            );
            assert!(
                !options.interactive,
                "native mode must force non-PTY options"
            );
            assert!(!options.acp, "native mode must remain one-shot stdio");
        }
    }
    Ok(())
}

#[test]
fn native_input_project_and_settings_mounts_have_distinct_authority() -> Result<()> {
    let fixture = Fixture::new()?;
    for agent in ["claude", "codex", "agy"] {
        let options = fixture.resolve(agent, &fixture.run_options(None))?;
        let project = options
            .overlays
            .iter()
            .find(|o| o.container_path == Path::new("/workspace"))
            .ok_or("project mount absent")?;
        assert_eq!(project.host_path, fixture.project);
        assert_eq!(project.permission, OverlayPermission::ReadWrite);
        let input = options
            .overlays
            .iter()
            .find(|o| o.container_path == Path::new("/workspace/input-1"))
            .ok_or("input mount absent")?;
        assert_eq!(input.host_path, fixture.input);
        assert_eq!(input.permission, OverlayPermission::ReadOnly);
        let settings: Vec<_> = options
            .overlays
            .iter()
            .filter(|o| {
                o.container_path != Path::new("/workspace")
                    && o.container_path != Path::new("/workspace/input-1")
            })
            .collect();
        assert!(
            !settings.is_empty(),
            "native mode must force isolated provider settings"
        );
        for mount in settings {
            assert!(
                !mount.host_path.starts_with(&fixture.home),
                "host settings must not be mounted directly"
            );
            assert_eq!(mount.permission, OverlayPermission::ReadWrite);
        }
        let argv = super::docker::build_run_argv(
            &ContainerName("native-test".into()),
            &ImageRef(IMAGE.into()),
            &options,
        );
        assert!(argv
            .iter()
            .any(|a| a == &format!("{}:/workspace/input-1:ro", fixture.input.display())));
        assert!(argv
            .iter()
            .any(|a| a == &format!("{}:/workspace", fixture.project.display())));
        assert!(!options.allow_docker);
        assert!(!argv.iter().any(|a| a.contains("docker.sock")));
    }
    assert_eq!(
        std::fs::read(fixture.input.join("review.txt"))?,
        b"readable input\n"
    );
    Ok(())
}

#[test]
fn native_missing_model_or_unsupported_agent_never_reaches_runtime() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut run = fixture.run_options(None);
    run.model = None;
    assert!(fixture.resolve("claude", &run).is_err());
    run.model = Some(" ".into());
    assert!(fixture.resolve("codex", &run).is_err());
    run.model = Some(MODEL.into());
    assert!(fixture.resolve("opencode", &run).is_err());
    run.native_session = Some(" ".into());
    assert!(fixture.resolve("agy", &run).is_err());
    Ok(())
}

#[test]
fn native_false_keeps_ordinary_interactive_agent_invocation() -> Result<()> {
    let fixture = Fixture::new()?;
    let run = AgentRunOptions {
        image_tag_override: Some(IMAGE.into()),
        ..Default::default()
    };
    let options = fixture.resolve("codex", &run)?;
    assert_eq!(provider_argv(&options)?, vec!["codex"]);
    assert!(options.interactive);
    Ok(())
}

struct CaptureFrontend {
    io: Option<AgentIo>,
}
impl UserMessageSink for CaptureFrontend {
    fn write_message(&mut self, _message: UserMessage) {}
    fn replay_queued(&mut self) {}
}
#[async_trait]
impl AgentFrontend for CaptureFrontend {
    fn report_status(&mut self, _status: AgentStatus) {}
    fn report_progress(&mut self, _progress: AgentProgress) {}
    fn take_io(&mut self) -> AgentIo {
        self.io.take().expect("take_io called once")
    }
}

async fn collect(mut rx: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(bytes) = rx.recv().await {
        out.extend(bytes);
    }
    out
}

async fn run_fake_container(
    agent: &str,
    exit_code: i32,
    output: bool,
) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>, i32)> {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new()?;
    let capture = fixture.project.join("stdin.bin");
    let script = fixture.project.join("container-cli");
    let stdout = if output {
        b"{\"result\":\"native\"}\n\n".repeat(8192)
    } else {
        Vec::new()
    };
    let stderr = if output {
        b"provider diagnostic\n".to_vec()
    } else {
        Vec::new()
    };
    let script_text = format!(
        "#!/usr/bin/env python3\nimport sys,os\nopen({capture:?},'wb').write(sys.stdin.buffer.read())\nfor _ in range({count}): os.write(1,b'{{\"result\":\"native\"}}\\n\\n')\nos.write(2,{stderr:?}.encode())\nsys.exit({exit_code})\n",
        capture=capture.to_string_lossy(), count=if output {8192} else {0}, stderr=if output {"provider diagnostic\n"} else {""},
    );
    std::fs::write(&script, script_text)?;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))?;
    let executable = Box::leak(script.to_string_lossy().into_owned().into_boxed_str());
    let options = fixture.resolve(agent, &fixture.run_options(None))?;
    let instance = ContainerInstance::new(
        ContainerCli::DOCKER.with_bin(executable),
        ImageRef(IMAGE.into()),
        ContainerName("native-fake".into()),
        options,
        Vec::new(),
        None,
    );
    let (stdout_tx, stdout_rx) = tokio::sync::mpsc::unbounded_channel();
    let (stderr_tx, stderr_rx) = tokio::sync::mpsc::unbounded_channel();
    let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
    let io = AgentIo {
        stdout: stdout_tx,
        stderr: stderr_tx,
        stdin_tx,
        stdin_rx,
        resize: None,
        initial_size: None,
    };
    let mut execution =
        Box::new(instance).run_with_frontend(Box::new(CaptureFrontend { io: Some(io) }))?;
    let exit = tokio::time::timeout(Duration::from_secs(8), execution.wait()).await??;
    drop(execution);
    let actual_stdout = tokio::time::timeout(Duration::from_secs(2), collect(stdout_rx)).await?;
    let actual_stderr = tokio::time::timeout(Duration::from_secs(2), collect(stderr_rx)).await?;
    assert_eq!(
        actual_stdout, stdout,
        "final stdout chunks must be fully drained"
    );
    assert_eq!(actual_stderr, stderr);
    Ok((
        actual_stdout,
        actual_stderr,
        std::fs::read(capture)?,
        exit.exit_code,
    ))
}

#[tokio::test]
async fn native_real_pipe_bridge_preserves_exact_provider_stdin_and_complete_streams() -> Result<()>
{
    for agent in ["claude", "codex", "agy"] {
        let (_, _, stdin, code) = run_fake_container(agent, 0, true).await?;
        assert_eq!(code, 0);
        assert_eq!(
            stdin,
            if agent == "codex" {
                PROMPT.as_bytes()
            } else {
                b""
            }
        );
    }
    Ok(())
}

#[tokio::test]
async fn native_zero_output_nonzero_exit_survives_the_real_pipe_bridge() -> Result<()> {
    let (stdout, stderr, stdin, code) = run_fake_container("codex", 7, false).await?;
    assert!(stdout.is_empty());
    assert!(stderr.is_empty());
    assert_eq!(stdin, PROMPT.as_bytes());
    assert_eq!(code, 7);
    Ok(())
}
