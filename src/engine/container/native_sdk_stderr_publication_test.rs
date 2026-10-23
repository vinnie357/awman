//! Issue 230: real stderr reads must establish clean native completion.

use super::gated_launch::{ChildLifecycleAuthority, LaunchRetentionRegistry};
use super::process::test_support::{
    spawn_native_fixture, spawn_native_fixture_with_stderr_read_adapter, FixtureIoMode,
    FixtureSpawnSpec,
};
use crate::data::fs::data_paths::DataPaths;
use crate::data::fs::native_sdk_state::{
    NativeSdkProvider, NativeSdkState, NativeSdkStateKey, NativeSdkStateLease,
};
use crate::engine::agent::agent_matrix::NativePrintRequest;
use crate::engine::agent_runtime::{AgentExecution, AgentExitInfo, CancelHandle};
use crate::engine::error::EngineError;
use std::cell::{Cell, RefCell};
use std::error::Error;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedReceiver;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;
type StderrReader = Box<dyn Read + Send>;
type Adapter = Box<dyn FnOnce(StderrReader) -> Option<StderrReader> + Send>;
type Identity = (u64, u64, u32, u32);
const IO_BOUND: Duration = Duration::from_secs(2);
const SESSION: &str = "stderr-contract-session";
const MODEL: &str = "stderr-contract-model";
const ACK: &[u8] = b"{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"session_id\":\"stderr-contract-session\",\"result\":\"ok\"}\n";
const STDERR: &[u8] = b"native stderr\0\xff\n";
const MUTATION: &[u8] = b"private SDK mutation";

fn identity(path: &Path) -> io::Result<Identity> {
    let m = fs::symlink_metadata(path)?;
    Ok((m.dev(), m.ino(), m.uid(), m.mode() & 0o7777))
}

fn private_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?
        .write_all(bytes)
}

struct Fixture {
    artifact: RefCell<Option<tempfile::TempDir>>,
    base: PathBuf,
    project: PathBuf,
    paths: DataPaths,
    sequence: Cell<usize>,
}

impl Fixture {
    fn new() -> TestResult<Self> {
        let home = dirs::home_dir().ok_or("test requires HOME")?;
        let artifact = tempfile::Builder::new()
            .prefix("awman-stderr-contract-")
            .tempdir_in(home)?;
        let base = artifact.path().to_path_buf();
        let project = base.join("project");
        let data = base.join("private-data");
        DirBuilder::new().mode(0o700).create(&project)?;
        DirBuilder::new().mode(0o700).create(&data)?;
        Ok(Self {
            artifact: RefCell::new(Some(artifact)),
            base,
            project,
            paths: DataPaths::at_root(data),
            sequence: Cell::new(0),
        })
    }

    fn preserve(&self) {
        if let Some(artifact) = self.artifact.borrow_mut().take() {
            let _ = artifact.keep();
        }
    }

    fn key(&self) -> TestResult<NativeSdkStateKey> {
        Ok(NativeSdkStateKey::new(
            &self.project,
            NativeSdkProvider::Claude,
            MODEL,
        )?)
    }

    fn associations(&self) -> TestResult<usize> {
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

fn take_receiver(receiver: &mut UnboundedReceiver<Vec<u8>>) -> UnboundedReceiver<Vec<u8>> {
    let (tx, empty) = tokio::sync::mpsc::unbounded_channel();
    drop(tx);
    std::mem::replace(receiver, empty)
}

async fn drain(receiver: &mut UnboundedReceiver<Vec<u8>>) -> TestResult<Vec<u8>> {
    let mut bytes = Vec::new();
    while let Some(chunk) = tokio::time::timeout(IO_BOUND, receiver.recv()).await? {
        bytes.extend(chunk);
    }
    Ok(bytes)
}

async fn receive_bytes(receiver: &mut UnboundedReceiver<Vec<u8>>, expected: &[u8]) -> TestResult {
    let mut bytes = Vec::new();
    while bytes.len() < expected.len() {
        bytes.extend(
            tokio::time::timeout(IO_BOUND, receiver.recv())
                .await?
                .ok_or("output closed before fixture bytes")?,
        );
    }
    assert_eq!(bytes, expected);
    Ok(())
}

struct NativeRun<'fixture> {
    fixture: &'fixture Fixture,
    authority: ChildLifecycleAuthority,
    cancel: Option<CancelHandle>,
    execution: Option<AgentExecution>,
    stdout: UnboundedReceiver<Vec<u8>>,
    stderr: UnboundedReceiver<Vec<u8>>,
    retained: PathBuf,
    before: Identity,
}

impl<'fixture> NativeRun<'fixture> {
    fn start(
        fixture: &'fixture Fixture,
        lease: NativeSdkStateLease,
        resume: bool,
        stderr: &[u8],
        adapter: Option<Adapter>,
    ) -> TestResult<Self> {
        let retained = lease.state_path().to_path_buf();
        let before = identity(&retained)?;
        let sequence = fixture.sequence.get();
        fixture.sequence.set(sequence + 1);
        let stdout_file = fixture.base.join(format!("stdout-{sequence}"));
        let stderr_file = fixture.base.join(format!("stderr-{sequence}"));
        private_file(&stdout_file, ACK)?;
        private_file(&stderr_file, stderr)?;
        let spec = FixtureSpawnSpec {
            mode: FixtureIoMode::Piped,
            executable: "/bin/sh",
            args: vec![
                "-c".into(),
                r#"printf 'private SDK mutation' > "$1"
/bin/cat "$2"
exec /bin/cat "$3" >&2
"#
                .into(),
                "native-stderr-fixture".into(),
                retained.join("sdk-mutation").to_string_lossy().into_owned(),
                stdout_file.to_string_lossy().into_owned(),
                stderr_file.to_string_lossy().into_owned(),
            ],
            seeded_prompt: None,
            grace_timeout: Duration::from_secs(10),
            stuck_timeout: Duration::from_secs(10),
            gate: None,
            fault: None,
        };
        let registry = LaunchRetentionRegistry::try_new()?;
        let request = NativePrintRequest::new("claude", Some(MODEL), resume.then_some(SESSION))?;
        let mut spawned = match adapter {
            Some(adapter) => spawn_native_fixture_with_stderr_read_adapter(
                spec,
                Arc::clone(&registry),
                request,
                lease,
                adapter,
            ),
            None => spawn_native_fixture(spec, Arc::clone(&registry), request, lease),
        };
        let stdout = take_receiver(&mut spawned.stdout);
        let stderr = take_receiver(&mut spawned.stderr);
        let Some(authority) = spawned.slot.authority() else {
            fixture.preserve();
            return Err("fixture lacks actual child authority".into());
        };
        let cancel = spawned
            .result
            .as_ref()
            .ok()
            .and_then(AgentExecution::cancel_handle);
        // Establish cleanup custody before inspecting the fallible public spawn result.
        let mut run = Self {
            fixture,
            authority,
            cancel,
            execution: None,
            stdout,
            stderr,
            retained,
            before,
        };
        run.execution = Some(spawned.finish(registry)?);
        Ok(run)
    }

    async fn wait(&mut self) -> TestResult<Result<AgentExitInfo, EngineError>> {
        Ok(tokio::time::timeout(
            IO_BOUND,
            self.execution
                .as_mut()
                .ok_or("execution was dropped")?
                .wait(),
        )
        .await?)
    }

    fn actual_zero(&self) -> TestResult {
        let actual = self
            .authority
            .wait_actual_until(Instant::now() + IO_BOUND)?
            .ok_or("actual child exit unresolved")?;
        assert_eq!(actual.exit_code, 0);
        assert_eq!(actual.signal, None);
        Ok(())
    }

    fn preserved_state(&self) -> TestResult {
        assert_eq!(identity(&self.retained)?, self.before);
        assert_eq!(fs::read(self.retained.join("sdk-mutation"))?, MUTATION);
        Ok(())
    }

    fn quiescent(&self) -> bool {
        matches!(
            self.authority.wait_actual_until(Instant::now()),
            Ok(Some(_))
        ) && self.authority.resources_are_released()
    }

    async fn await_retirement(&self) -> TestResult {
        tokio::time::timeout(IO_BOUND, async {
            while !self.quiescent() {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        Ok(())
    }
}

impl Drop for NativeRun<'_> {
    fn drop(&mut self) {
        drop(self.execution.take());
        if !self.quiescent() {
            if let Some(cancel) = &self.cancel {
                let _ = cancel.cancel();
            } else {
                let _ = self
                    .authority
                    .terminate_local_cli(Instant::now() + IO_BOUND);
            }
        }
        if !self.quiescent() {
            self.fixture.preserve();
        }
    }
}

#[derive(Default)]
struct Witness {
    interrupts: AtomicUsize,
    delegated_after_interrupt: AtomicUsize,
    real_bytes: AtomicUsize,
    actual_eofs: AtomicUsize,
    returned_eofs: AtomicUsize,
    hard_errors: AtomicUsize,
}

struct ReadGate {
    entered: tokio::sync::mpsc::UnboundedSender<()>,
    release: mpsc::Receiver<()>,
}

impl ReadGate {
    fn pause(self) -> io::Result<()> {
        self.entered
            .send(())
            .map_err(|_| io::Error::other("gate observer dropped"))?;
        self.release
            .recv()
            .map_err(|_| io::Error::other("gate release dropped"))
    }
}

struct ReleaseGuard(Option<mpsc::Sender<()>>);
impl ReleaseGuard {
    fn release(&mut self) {
        if let Some(tx) = self.0.take() {
            let _ = tx.send(());
        }
    }
}
impl Drop for ReleaseGuard {
    fn drop(&mut self) {
        self.release();
    }
}

fn read_gate() -> (ReadGate, ReleaseGuard, UnboundedReceiver<()>) {
    let (entered, receiver) = tokio::sync::mpsc::unbounded_channel();
    let (release, wait) = mpsc::channel();
    (
        ReadGate {
            entered,
            release: wait,
        },
        ReleaseGuard(Some(release)),
        receiver,
    )
}

struct Probe {
    inner: StderrReader,
    witness: Arc<Witness>,
    interrupt_first: bool,
    fail_at_eof: bool,
    before_read: Option<ReadGate>,
    at_eof: Option<ReadGate>,
}

impl Read for Probe {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.interrupt_first {
            self.interrupt_first = false;
            self.witness.interrupts.fetch_add(1, Ordering::SeqCst);
            return Err(io::Error::from(io::ErrorKind::Interrupted));
        }
        if let Some(gate) = self.before_read.take() {
            gate.pause()?;
        }
        if self.witness.interrupts.load(Ordering::SeqCst) != 0 {
            self.witness
                .delegated_after_interrupt
                .fetch_add(1, Ordering::SeqCst);
        }
        let result = self.inner.read(buf);
        if let Ok(n) = &result {
            self.witness.real_bytes.fetch_add(*n, Ordering::SeqCst);
            if *n == 0 {
                self.witness.actual_eofs.fetch_add(1, Ordering::SeqCst);
                if let Some(gate) = self.at_eof.take() {
                    gate.pause()?;
                }
                if self.fail_at_eof {
                    self.witness.hard_errors.fetch_add(1, Ordering::SeqCst);
                    return Err(io::Error::other("injected stderr failure after actual EOF"));
                }
                self.witness.returned_eofs.fetch_add(1, Ordering::SeqCst);
            }
        }
        result
    }
}

fn adapter(
    witness: &Arc<Witness>,
    interrupt_first: bool,
    fail_at_eof: bool,
    before_read: Option<ReadGate>,
    at_eof: Option<ReadGate>,
) -> Adapter {
    let witness = Arc::clone(witness);
    Box::new(move |inner| {
        Some(Box::new(Probe {
            inner,
            witness,
            interrupt_first,
            fail_at_eof,
            before_read,
            at_eof,
        }))
    })
}

fn assert_resumes_original(
    state: &NativeSdkState,
    key: &NativeSdkStateKey,
    run: &NativeRun<'_>,
) -> TestResult {
    let lease = state.acquire_resume(key, SESSION)?;
    assert_eq!(lease.state_path(), run.retained);
    assert_eq!(identity(lease.state_path())?, run.before);
    lease.validate()?;
    run.preserved_state()
}

#[tokio::test]
async fn clean_empty_real_stderr_allows_native_publication() -> TestResult {
    let fixture = Fixture::new()?;
    let state = NativeSdkState::new(&fixture.paths)?;
    let key = fixture.key()?;
    let mut run = NativeRun::start(&fixture, state.acquire_first(&key)?, false, b"", None)?;
    assert_eq!(run.wait().await??.exit_code, 0);
    run.actual_zero()?;
    assert!(run.authority.resources_are_released());
    assert_eq!(drain(&mut run.stdout).await?, ACK);
    assert!(drain(&mut run.stderr).await?.is_empty());
    assert_eq!(fixture.associations()?, 1);
    assert_resumes_original(&state, &key, &run)
}

#[tokio::test]
async fn interrupted_before_first_real_stderr_read_retries_and_waits_for_actual_drain() -> TestResult
{
    let fixture = Fixture::new()?;
    let state = NativeSdkState::new(&fixture.paths)?;
    let key = fixture.key()?;
    let witness = Arc::new(Witness::default());
    let (gate, mut release, mut entered) = read_gate();
    let mut run = NativeRun::start(
        &fixture,
        state.acquire_first(&key)?,
        false,
        STDERR,
        Some(adapter(&witness, true, false, None, Some(gate))),
    )?;
    tokio::time::timeout(IO_BOUND, entered.recv())
        .await?
        .ok_or("stderr did not reach actual EOF after Interrupted")?;
    receive_bytes(&mut run.stdout, ACK).await?;
    receive_bytes(&mut run.stderr, STDERR).await?;
    run.actual_zero()?;
    assert_eq!(witness.interrupts.load(Ordering::SeqCst), 1);
    assert!(witness.delegated_after_interrupt.load(Ordering::SeqCst) >= 2);
    assert_eq!(witness.real_bytes.load(Ordering::SeqCst), STDERR.len());
    assert_eq!(witness.actual_eofs.load(Ordering::SeqCst), 1);
    assert_eq!(witness.returned_eofs.load(Ordering::SeqCst), 0);
    assert!(!run.authority.resources_are_released());
    assert_eq!(fixture.associations()?, 0);
    assert!(state.acquire_resume(&key, SESSION).is_err());
    run.preserved_state()?;
    release.release();
    assert_eq!(run.wait().await??.exit_code, 0);
    run.actual_zero()?;
    assert!(run.authority.resources_are_released());
    assert!(drain(&mut run.stdout).await?.is_empty());
    assert!(drain(&mut run.stderr).await?.is_empty());
    assert_eq!(witness.returned_eofs.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.associations()?, 1);
    assert_resumes_original(&state, &key, &run)
}

#[tokio::test]
async fn hard_stderr_error_after_real_eof_rejects_native_publication_despite_actual_zero(
) -> TestResult {
    let fixture = Fixture::new()?;
    let state = NativeSdkState::new(&fixture.paths)?;
    let key = fixture.key()?;
    let witness = Arc::new(Witness::default());
    let mut run = NativeRun::start(
        &fixture,
        state.acquire_first(&key)?,
        false,
        STDERR,
        Some(adapter(&witness, false, true, None, None)),
    )?;
    let completion = run.wait().await?;
    run.actual_zero()?;
    assert!(
        completion.is_err(),
        "hard stderr failure must reject public native completion"
    );
    assert!(run.authority.resources_are_released());
    assert_eq!(drain(&mut run.stdout).await?, ACK);
    assert_eq!(drain(&mut run.stderr).await?, STDERR);
    assert_eq!(witness.real_bytes.load(Ordering::SeqCst), STDERR.len());
    assert_eq!(witness.actual_eofs.load(Ordering::SeqCst), 1);
    assert_eq!(witness.hard_errors.load(Ordering::SeqCst), 1);
    assert_eq!(witness.returned_eofs.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.associations()?, 0);
    assert!(state.acquire_resume(&key, SESSION).is_err());
    run.preserved_state()
}

#[tokio::test]
async fn removed_required_stderr_rejects_native_publication_despite_actual_zero() -> TestResult {
    let fixture = Fixture::new()?;
    let state = NativeSdkState::new(&fixture.paths)?;
    let key = fixture.key()?;
    let removed = Arc::new(AtomicUsize::new(0));
    let called = Arc::clone(&removed);
    let remove: Adapter = Box::new(move |real_pipe| {
        drop(real_pipe);
        called.fetch_add(1, Ordering::SeqCst);
        None
    });
    let mut run = NativeRun::start(
        &fixture,
        state.acquire_first(&key)?,
        false,
        b"",
        Some(remove),
    )?;
    let completion = run.wait().await?;
    run.actual_zero()?;
    assert!(
        completion.is_err(),
        "a missing required stderr pipe must reject native completion"
    );
    assert_eq!(removed.load(Ordering::SeqCst), 1);
    assert!(run.authority.resources_are_released());
    assert_eq!(drain(&mut run.stdout).await?, ACK);
    assert!(drain(&mut run.stderr).await?.is_empty());
    assert_eq!(fixture.associations()?, 0);
    assert!(state.acquire_resume(&key, SESSION).is_err());
    run.preserved_state()
}

#[tokio::test]
async fn dropped_stderr_sink_still_drains_real_pipe_to_clean_native_completion() -> TestResult {
    let fixture = Fixture::new()?;
    let state = NativeSdkState::new(&fixture.paths)?;
    let key = fixture.key()?;
    let payload = vec![b'z'; 200_000];
    let witness = Arc::new(Witness::default());
    let (gate, mut release, mut entered) = read_gate();
    let mut run = NativeRun::start(
        &fixture,
        state.acquire_first(&key)?,
        false,
        &payload,
        Some(adapter(&witness, false, false, Some(gate), None)),
    )?;
    tokio::time::timeout(IO_BOUND, entered.recv())
        .await?
        .ok_or("stderr reader did not enter sink-drop gate")?;
    drop(take_receiver(&mut run.stderr));
    release.release();
    assert_eq!(run.wait().await??.exit_code, 0);
    run.actual_zero()?;
    assert!(run.authority.resources_are_released());
    assert_eq!(drain(&mut run.stdout).await?, ACK);
    assert_eq!(witness.real_bytes.load(Ordering::SeqCst), payload.len());
    assert_eq!(witness.actual_eofs.load(Ordering::SeqCst), 1);
    assert_eq!(witness.returned_eofs.load(Ordering::SeqCst), 1);
    assert_eq!(witness.hard_errors.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.associations()?, 1);
    assert_resumes_original(&state, &key, &run)
}

#[tokio::test]
async fn dropping_resumed_execution_holds_writer_lock_until_real_stderr_reader_retires(
) -> TestResult {
    let fixture = Fixture::new()?;
    let state = NativeSdkState::new(&fixture.paths)?;
    let key = fixture.key()?;
    let mut first = NativeRun::start(&fixture, state.acquire_first(&key)?, false, b"", None)?;
    assert_eq!(first.wait().await??.exit_code, 0);
    first.actual_zero()?;
    assert!(first.authority.resources_are_released());
    assert_eq!(drain(&mut first.stdout).await?, ACK);
    assert!(drain(&mut first.stderr).await?.is_empty());
    assert_eq!(fixture.associations()?, 1);
    let resume = state.acquire_resume(&key, SESSION)?;
    assert_eq!(resume.state_path(), first.retained);
    assert_eq!(identity(resume.state_path())?, first.before);
    let witness = Arc::new(Witness::default());
    let (gate, mut release, mut entered) = read_gate();
    let mut resumed = NativeRun::start(
        &fixture,
        resume,
        true,
        STDERR,
        Some(adapter(&witness, false, false, Some(gate), None)),
    )?;
    tokio::time::timeout(IO_BOUND, entered.recv())
        .await?
        .ok_or("resumed stderr reader did not enter gate")?;
    receive_bytes(&mut resumed.stdout, ACK).await?;
    resumed.actual_zero()?;
    assert!(!resumed.authority.resources_are_released());
    assert!(state.acquire_resume(&key, SESSION).is_err());
    drop(resumed.execution.take());
    // The reader remains owned after caller drop, before this guard releases it.
    assert!(!resumed.authority.resources_are_released());
    assert!(state.acquire_resume(&key, SESSION).is_err());
    assert_eq!(witness.actual_eofs.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.associations()?, 1);
    resumed.preserved_state()?;
    release.release();
    resumed.await_retirement().await?;
    resumed.actual_zero()?;
    assert!(drain(&mut resumed.stdout).await?.is_empty());
    assert_eq!(drain(&mut resumed.stderr).await?, STDERR);
    assert_eq!(witness.real_bytes.load(Ordering::SeqCst), STDERR.len());
    assert_eq!(witness.actual_eofs.load(Ordering::SeqCst), 1);
    assert_eq!(witness.returned_eofs.load(Ordering::SeqCst), 1);
    assert_resumes_original(&state, &key, &resumed)
}
