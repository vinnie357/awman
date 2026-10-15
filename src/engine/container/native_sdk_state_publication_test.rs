//! Issue 228: actual piped-child ownership, ACK observation, and publication.
//! Exact AW container cleanup and retained recovery require the next frozen slice.

use super::gated_launch::{ChildLifecycleAuthority, LaunchRetentionRegistry};
use super::process::test_support::{spawn_native_fixture, FixtureIoMode, FixtureSpawnSpec};
use crate::data::fs::data_paths::DataPaths;
use crate::data::fs::native_sdk_state::{NativeSdkProvider, NativeSdkState, NativeSdkStateKey};
use crate::engine::agent::agent_matrix::NativePrintRequest;
use crate::engine::agent_runtime::{AgentExecution, CancelHandle};
use std::cell::RefCell;
use std::error::Error;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedReceiver;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;
type FilesystemIdentity = (u64, u64, u32, u32);
const IO_BOUND: Duration = Duration::from_secs(2);
const SESSION: &str = "native-owner-session";
const MODEL: &str = "explicit-native-model";
const MUTATION: &[u8] = b"private SDK mutation";
const STDERR: &[u8] = b"native stderr\0\xff\n";

struct Fixture {
    _home_artifact: RefCell<Option<tempfile::TempDir>>,
    base: PathBuf,
    project: PathBuf,
    paths: DataPaths,
}

fn private_dir(path: &Path) -> io::Result<()> {
    DirBuilder::new().mode(0o700).create(path)
}

fn private_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?
        .write_all(bytes)
}

fn identity(path: &Path) -> io::Result<(u64, u64, u32, u32)> {
    let metadata = fs::symlink_metadata(path)?;
    Ok((
        metadata.dev(),
        metadata.ino(),
        metadata.uid(),
        metadata.mode() & 0o7777,
    ))
}

impl Fixture {
    fn new() -> TestResult<Self> {
        let home = dirs::home_dir().ok_or_else(|| io::Error::other("test requires HOME"))?;
        let artifact = tempfile::Builder::new()
            .prefix("awman-native-publication-contract-")
            .tempdir_in(home)?;
        let base = artifact.path().to_path_buf();
        let project = base.join("project");
        let root = base.join("private-aw-data");
        private_dir(&project)?;
        private_dir(&root)?;
        Ok(Self {
            _home_artifact: RefCell::new(Some(artifact)),
            base,
            project,
            paths: DataPaths::at_root(root),
        })
    }

    fn preserve_on_uncertainty(&self) {
        if let Some(artifact) = self._home_artifact.borrow_mut().take() {
            let _ = artifact.keep();
        }
    }

    fn key(&self, provider: NativeSdkProvider) -> TestResult<NativeSdkStateKey> {
        Ok(NativeSdkStateKey::new(&self.project, provider, MODEL)?)
    }

    fn association_count(&self) -> io::Result<usize> {
        let mut count = 0;
        for namespace in fs::read_dir(self.paths.root().join("native-sdk-state"))? {
            let sessions = namespace?.path().join("sessions");
            if sessions.is_dir() {
                count += fs::read_dir(sessions)?
                    .collect::<io::Result<Vec<_>>>()?
                    .len();
            }
        }
        Ok(count)
    }
}

fn provider_cases() -> [(NativeSdkProvider, &'static str, Vec<u8>); 3] {
    // Located native schemas: Altana council_headless_protocol.zig:433–449,
    // 566–574, 775–811. These are SDK stdout, not AW startup-gate ACKs.
    [
        (
            NativeSdkProvider::Claude,
            "claude",
            format!(
                "{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"session_id\":\"{SESSION}\",\"result\":\"review λ\"}}\n"
            )
            .into_bytes(),
        ),
        (
            NativeSdkProvider::Codex,
            "codex",
            format!(
                "{{\"type\":\"thread.started\",\"thread_id\":\"{SESSION}\"}}\n{{\"type\":\"turn.started\"}}\n{{\"type\":\"item.completed\",\"item\":{{\"id\":\"message-1\",\"type\":\"agent_message\",\"text\":\"review λ\"}}}}\n{{\"type\":\"turn.completed\",\"usage\":{{\"input_tokens\":1,\"cached_input_tokens\":0,\"output_tokens\":1}}}}\n"
            )
            .into_bytes(),
        ),
        (
            NativeSdkProvider::Agy,
            "agy",
            format!(
                "{{\"status\":\"SUCCESS\",\"response\":\"review λ\",\"conversation_id\":\"{SESSION}\"}}\n"
            )
            .into_bytes(),
        ),
    ]
}

fn spawn_spec(args: Vec<String>) -> FixtureSpawnSpec {
    FixtureSpawnSpec {
        mode: FixtureIoMode::Piped,
        executable: "/bin/sh",
        args,
        seeded_prompt: None,
        grace_timeout: Duration::from_secs(10),
        stuck_timeout: Duration::from_secs(10),
        gate: None,
        fault: None,
    }
}

fn take_receiver(receiver: &mut UnboundedReceiver<Vec<u8>>) -> UnboundedReceiver<Vec<u8>> {
    let (sender, empty) = tokio::sync::mpsc::unbounded_channel();
    drop(sender);
    std::mem::replace(receiver, empty)
}

async fn drain(receiver: &mut UnboundedReceiver<Vec<u8>>) -> TestResult<Vec<u8>> {
    let mut bytes = Vec::new();
    loop {
        match tokio::time::timeout(IO_BOUND, receiver.recv()).await? {
            Some(chunk) => bytes.extend(chunk),
            None => return Ok(bytes),
        }
    }
}

struct NativeFixtureGuard<'fixture> {
    fixture: &'fixture Fixture,
    lifecycle: Option<ChildLifecycleAuthority>,
    cancel: Option<CancelHandle>,
}

impl NativeFixtureGuard<'_> {
    fn require_settled_zero_exit(&self) -> TestResult {
        let authority = self
            .lifecycle
            .as_ref()
            .ok_or("native fixture has no actual lifecycle authority")?;
        let info = authority
            .wait_actual_until(Instant::now())?
            .ok_or("native fixture child exit remains unresolved")?;
        assert_eq!(info.exit_code, 0);
        if !authority.resources_are_released() {
            return Err("native fixture readers/resources remain owned".into());
        }
        Ok(())
    }

    fn child_and_resources_are_quiescent(&self) -> bool {
        self.lifecycle.as_ref().is_some_and(|authority| {
            matches!(authority.wait_actual_until(Instant::now()), Ok(Some(_)))
                && authority.resources_are_released()
        })
    }
}

impl Drop for NativeFixtureGuard<'_> {
    fn drop(&mut self) {
        if !self.child_and_resources_are_quiescent() {
            if let Some(cancel) = &self.cancel {
                let _ = cancel.cancel();
            } else if let Some(authority) = &self.lifecycle {
                let _ = authority.terminate_local_cli(Instant::now() + IO_BOUND);
            }
        }
        // Cancellation is a request; unresolved native custody keeps HOME paths.
        if !self.child_and_resources_are_quiescent() {
            self.fixture.preserve_on_uncertainty();
        }
    }
}

struct LiveSdk<'fixture> {
    commands: File,
    execution: AgentExecution,
    stdout: UnboundedReceiver<Vec<u8>>,
    stderr: UnboundedReceiver<Vec<u8>>,
    custody: NativeFixtureGuard<'fixture>,
}

impl Drop for LiveSdk<'_> {
    fn drop(&mut self) {
        let _ = self.commands.write_all(b"finish\n");
    }
}

impl<'fixture> LiveSdk<'fixture> {
    fn start(
        fixture: &'fixture Fixture,
        state: &NativeSdkState,
        key: &NativeSdkStateKey,
        agent: &str,
    ) -> TestResult<(Self, PathBuf, FilesystemIdentity)> {
        let lease = state.acquire_first(key)?;
        let state_path = lease.state_path().to_path_buf();
        let state_identity = identity(&state_path)?;
        let fifo = fixture.base.join("sdk-commands.fifo");
        nix::unistd::mkfifo(
            &fifo,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )?;
        let commands = OpenOptions::new().read(true).write(true).open(&fifo)?;
        let stderr_file = fixture.base.join("native-stderr");
        private_file(&stderr_file, STDERR)?;
        // One command per byte forces a real reader boundary, including split UTF-8.
        // The child cannot exit until the test sends finish through its private FIFO.
        let script = r#"exec 3< "$1"
printf 'private SDK mutation' > "$2"
while IFS= read -r octal <&3; do
    case "$octal" in
        finish) break ;;
        *) printf '%b' "\\$octal" ;;
    esac
done
cat "$3" >&2
"#;
        let registry = LaunchRetentionRegistry::try_new()?;
        let mut run = spawn_native_fixture(
            spawn_spec(vec![
                "-c".into(),
                script.into(),
                "native-sdk-fixture".into(),
                fifo.to_string_lossy().into_owned(),
                state_path
                    .join("sdk-mutation")
                    .to_string_lossy()
                    .into_owned(),
                stderr_file.to_string_lossy().into_owned(),
            ]),
            Arc::clone(&registry),
            NativePrintRequest::new(agent, Some(MODEL), None)?,
            lease,
        );
        let stdout = take_receiver(&mut run.stdout);
        let stderr = take_receiver(&mut run.stderr);
        let custody = NativeFixtureGuard {
            fixture,
            lifecycle: run.slot.authority(),
            cancel: run
                .result
                .as_ref()
                .ok()
                .and_then(AgentExecution::cancel_handle),
        };
        let execution = run.finish(registry)?;
        Ok((
            Self {
                commands,
                execution,
                stdout,
                stderr,
                custody,
            },
            state_path,
            state_identity,
        ))
    }

    async fn emit_fragmented(&mut self, payload: &[u8]) -> TestResult<Vec<u8>> {
        let mut forwarded = Vec::new();
        for expected in payload {
            writeln!(&mut self.commands, "{expected:03o}")?;
            let chunk = tokio::time::timeout(IO_BOUND, self.stdout.recv())
                .await?
                .ok_or("stdout closed before the requested fragment")?;
            assert_eq!(chunk.as_slice(), &[*expected]);
            forwarded.extend(chunk);
        }
        Ok(forwarded)
    }

    async fn finish(&mut self) -> TestResult {
        self.commands.write_all(b"finish\n")?;
        let info = self.execution.wait().await?;
        assert_eq!(info.exit_code, 0);
        self.custody.require_settled_zero_exit()
    }
}

#[tokio::test]
async fn successful_empty_stdout_never_publishes_a_resume_association() -> TestResult {
    for (provider, agent, _) in provider_cases() {
        let fixture = Fixture::new()?;
        let state = NativeSdkState::new(&fixture.paths)?;
        let key = fixture.key(provider)?;
        let lease = state.acquire_first(&key)?;
        let retained = lease.state_path().to_path_buf();
        let before = identity(&retained)?;
        let registry = LaunchRetentionRegistry::try_new()?;
        let mut run = spawn_native_fixture(
            spawn_spec(vec!["-c".into(), "exit 0".into()]),
            Arc::clone(&registry),
            NativePrintRequest::new(agent, Some(MODEL), None)?,
            lease,
        );
        let mut stdout = take_receiver(&mut run.stdout);
        let custody = NativeFixtureGuard {
            fixture: &fixture,
            lifecycle: run.slot.authority(),
            cancel: run
                .result
                .as_ref()
                .ok()
                .and_then(AgentExecution::cancel_handle),
        };
        let mut execution = run.finish(registry)?;
        let info = execution.wait().await?;
        assert_eq!(info.exit_code, 0);
        assert!(drain(&mut stdout).await?.is_empty());
        custody.require_settled_zero_exit()?;
        drop(execution);
        assert_eq!(fixture.association_count()?, 0);
        assert!(state.acquire_resume(&key, SESSION).is_err());
        assert_eq!(identity(&retained)?, before);
    }
    Ok(())
}

#[tokio::test]
async fn fragmented_provider_ack_publishes_original_state_after_actual_wait_and_drain() -> TestResult
{
    for (provider, agent, payload) in provider_cases() {
        let fixture = Fixture::new()?;
        let state = NativeSdkState::new(&fixture.paths)?;
        let key = fixture.key(provider)?;
        let (mut sdk, retained, before) = LiveSdk::start(&fixture, &state, &key, agent)?;
        let mut forwarded = sdk.emit_fragmented(&payload).await?;
        sdk.finish().await?;
        forwarded.extend(drain(&mut sdk.stdout).await?);
        assert_eq!(forwarded, payload);
        assert_eq!(drain(&mut sdk.stderr).await?, STDERR);
        assert_eq!(fixture.association_count()?, 1);
        drop(sdk);
        drop(state);
        let later_state = NativeSdkState::new(&fixture.paths)?;
        let resumed = later_state.acquire_resume(&key, SESSION)?;
        assert_eq!(resumed.state_path(), retained);
        assert_eq!(identity(resumed.state_path())?, before);
        assert_eq!(
            fs::read(resumed.state_path().join("sdk-mutation"))?,
            MUTATION
        );
        resumed.validate()?;
    }
    Ok(())
}

#[tokio::test]
async fn complete_provider_ack_while_child_is_live_cannot_publish_or_authorize_resume() -> TestResult
{
    for (provider, agent, payload) in provider_cases() {
        let fixture = Fixture::new()?;
        let state = NativeSdkState::new(&fixture.paths)?;
        let key = fixture.key(provider)?;
        let (mut sdk, retained, before) = LiveSdk::start(&fixture, &state, &key, agent)?;
        assert_eq!(fixture.association_count()?, 0);
        assert_eq!(sdk.emit_fragmented(&payload).await?, payload);
        // Every byte was observed; the child still blocks on its finish command.
        assert_eq!(fixture.association_count()?, 0);
        assert!(state.acquire_resume(&key, SESSION).is_err());
        assert_eq!(identity(&retained)?, before);
        assert_eq!(fs::read(retained.join("sdk-mutation"))?, MUTATION);
        sdk.finish().await?;
        assert!(drain(&mut sdk.stdout).await?.is_empty());
        assert_eq!(drain(&mut sdk.stderr).await?, STDERR);
        assert_eq!(fixture.association_count()?, 1);
        drop(sdk);
        state.acquire_resume(&key, SESSION)?.validate()?;
    }
    Ok(())
}
