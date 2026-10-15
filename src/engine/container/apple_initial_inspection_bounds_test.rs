use super::gated_launch::{MAX_PROVIDER_CUSTODY_SHUTDOWN, MAX_PROVIDER_STDOUT};
use super::runtime::InitialInspectionOutcomeForTest;
use super::{ContainerName, ContainerOption, ContainerRuntime, ImageRef};
use crate::data::startup_gate::load_startup_gate;
use crate::engine::container::options::ResolvedContainerOptions;
use crate::engine::error::EngineError;
use sha2::{Digest, Sha256};
use std::error::Error;
use std::fs::OpenOptions;
use std::io::{Read as _, Write as _};
use std::net::Shutdown;
use std::os::fd::AsFd as _;
use std::os::unix::fs::{symlink, OpenOptionsExt as _, PermissionsExt as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const NAME: &str = "awman-altana-initial-image-inspection-bounds";
const IMAGE: &str = "awman-initial-image-inspection:latest";
const TOKEN: &str = "5555555555555555555555555555555555555555555555555555555555555555";
const CHILD_TEST: &str =
    "engine::container::apple_initial_inspection_bounds_test::image_inspection_fixture_child";
const CHILD_MODE_ENV: &str = "AWMAN_INITIAL_IMAGE_INSPECTION_MODE";
const CHILD_SOCKET_ENV: &str = "AWMAN_INITIAL_IMAGE_INSPECTION_SOCKET";
const HANDSHAKE_ACK: u8 = 1;
const RELEASE: u8 = 2;
const CHILD_READY: u8 = 3;

#[derive(Clone, Copy)]
enum FixtureMode {
    Block,
    StdoutOverflow,
}

impl FixtureMode {
    fn name(self) -> &'static str {
        match self {
            Self::Block => "block",
            Self::StdoutOverflow => "stdout-overflow",
        }
    }
}

struct GateFixture {
    _root: tempfile::TempDir,
    parent: PathBuf,
}

impl GateFixture {
    fn new() -> Result<Self, Box<dyn Error>> {
        let root = tempfile::tempdir()?;
        let parent = root.path().join("control-parent");
        let guest = parent.join("guest-control");
        std::fs::create_dir(&parent)?;
        std::fs::create_dir(&guest)?;
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700))?;
        std::fs::set_permissions(&guest, std::fs::Permissions::from_mode(0o700))?;

        let manifest = b"{\"version\":1,\"entries\":[]}\n";
        let manifest_digest = lower_hex(&Sha256::digest(manifest));
        write_private(&parent.join("workspace.manifest.json"), manifest)?;
        write_private(
            &parent.join("request.json"),
            format!(
                "{{\"version\":1,\"bindings\":[{{\"id\":\"workspace\",\"workspace_path\":\"/workspace\",\"manifest_id\":\"{manifest_digest}\",\"manifest_file\":\"workspace.manifest.json\",\"access\":\"read-only\"}}]}}\n"
            )
            .as_bytes(),
        )?;
        write_private(
            &parent.join("launch-intent.json"),
            format!(
                "{{\"version\":1,\"container_name\":\"{NAME}\",\"launch_token\":\"{TOKEN}\"}}\n"
            )
            .as_bytes(),
        )?;
        Ok(Self {
            _root: root,
            parent,
        })
    }

    fn resolved_options(
        &self,
        timeout: Duration,
    ) -> Result<ResolvedContainerOptions, Box<dyn Error>> {
        let gate = load_startup_gate(&self.parent, timeout)?;
        let (_, identity) = gate
            .control
            .orchestrated_parts()
            .ok_or("expected orchestrated startup gate")?;
        let label = lower_hex(&identity.token_digest);
        Ok(ResolvedContainerOptions::resolve([
            ContainerOption::Image(ImageRef::new(IMAGE)),
            ContainerOption::Name(ContainerName::new(NAME)),
            ContainerOption::Label {
                key: "dev.awman.orchestrator-launch".into(),
                value: label,
            },
            ContainerOption::KeepContainer,
            ContainerOption::StartupGate(gate),
            ContainerOption::StartupGateTrustedTemplate,
        ])?)
    }
}

struct ProviderFixture {
    _root: tempfile::TempDir,
    executable: PathBuf,
    listener: Option<ListeningSocket>,
}

struct ListeningSocket {
    listener: UnixListener,
    path: PathBuf,
}

impl Drop for ListeningSocket {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl ProviderFixture {
    fn new(mode: FixtureMode) -> Result<Self, Box<dyn Error>> {
        let root = tempfile::tempdir()?;
        let socket = root.path().join("control.sock");
        let listener = UnixListener::bind(&socket)?;
        let test_binary = root.path().join("test-binary");
        symlink(std::env::current_exe()?, &test_binary)?;
        let executable = root.path().join("container");
        let script = format!(
            "#!/bin/sh\n\
if [ \"$#\" -ne 3 ] || [ \"$1\" != image ] || [ \"$2\" != inspect ] || [ \"$3\" != '{IMAGE}' ]; then\n\
  exit 64\n\
fi\n\
{CHILD_MODE_ENV}='{}' {CHILD_SOCKET_ENV}=\"${{0%/*}}/control.sock\" \\\nexec \"${{0%/*}}/test-binary\" --exact '{CHILD_TEST}' --ignored --nocapture\n",
            mode.name()
        );
        std::fs::write(&executable, script)?;
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))?;
        Ok(Self {
            _root: root,
            executable,
            listener: Some(ListeningSocket {
                listener,
                path: socket,
            }),
        })
    }

    fn accept_child(&mut self) -> Result<ChildLatch, Box<dyn Error>> {
        let listener = self
            .listener
            .take()
            .ok_or("provider fixture listener is unavailable")?;
        if !wait_until_readable(
            &listener.listener,
            Instant::now() + MAX_PROVIDER_CUSTODY_SHUTDOWN,
        )? {
            return Err("initial image inspection did not start its provider child".into());
        }
        let (stream, _) = listener.listener.accept()?;
        drop(listener);
        ChildLatch::new(stream)
    }

    fn close_listener(&mut self) {
        self.listener.take();
    }
}

fn wait_until_readable(listener: &UnixListener, deadline: Instant) -> std::io::Result<bool> {
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(false);
        }
        let milliseconds = remaining.as_millis().clamp(1, u128::from(u16::MAX)) as u16;
        let mut descriptors = [PollFd::new(listener.as_fd(), PollFlags::POLLIN)];
        match poll(&mut descriptors, PollTimeout::from(milliseconds)) {
            Ok(0) => continue,
            Ok(_) => return Ok(true),
            Err(nix::errno::Errno::EINTR) => continue,
            Err(error) => return Err(std::io::Error::from(error)),
        }
    }
}

fn lower_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

struct ChildLatch {
    stream: UnixStream,
}

impl ChildLatch {
    fn new(stream: UnixStream) -> Result<Self, Box<dyn Error>> {
        let control = Self { stream };
        control
            .stream
            .set_read_timeout(Some(MAX_PROVIDER_CUSTODY_SHUTDOWN))?;
        control
            .stream
            .set_write_timeout(Some(MAX_PROVIDER_CUSTODY_SHUTDOWN))?;
        Ok(control)
    }

    fn child_pid(&mut self) -> Result<u32, Box<dyn Error>> {
        let mut bytes = [0_u8; 4];
        self.stream.read_exact(&mut bytes)?;
        self.stream.write_all(&[HANDSHAKE_ACK])?;
        self.stream.flush()?;
        let mut ready = [0_u8; 1];
        self.stream.read_exact(&mut ready)?;
        if ready != [CHILD_READY] {
            return Err("invalid provider fixture readiness".into());
        }
        Ok(u32::from_ne_bytes(bytes))
    }

    fn release(&mut self) {
        let _ = self.stream.write_all(&[RELEASE]);
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

struct ProviderRun {
    fixture: ProviderFixture,
    control: Option<ChildLatch>,
    build: Option<JoinHandle<()>>,
}

impl ProviderRun {
    fn release_and_join(&mut self) -> Result<(), Box<dyn Error>> {
        self.control.take();
        self.fixture.close_listener();
        if let Some(build) = self.build.take() {
            build
                .join()
                .map_err(|_| "public initial image inspection worker panicked")?;
        }
        Ok(())
    }
}

impl Drop for ProviderRun {
    fn drop(&mut self) {
        self.control.take();
        self.fixture.close_listener();
        if let Some(build) = self.build.take() {
            let _ = build.join();
        }
    }
}

impl Drop for ChildLatch {
    fn drop(&mut self) {
        self.release();
    }
}

fn process_is_reaped(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None) == Err(nix::errno::Errno::ESRCH)
}

fn exercise_public_initial_inspection(
    mode: FixtureMode,
    gate_timeout: Duration,
    response_bound: Duration,
) -> Result<(), Box<dyn Error>> {
    let gate = GateFixture::new()?;
    let options = gate.resolved_options(gate_timeout)?;
    let fixture = ProviderFixture::new(mode)?;
    let receipt = Arc::new(Mutex::new(None));
    let mut runtime =
        ContainerRuntime::apple_with_provider_executable_for_test(&fixture.executable);
    runtime.set_initial_inspection_receipt_for_test(Arc::clone(&receipt));
    let (sender, receiver) = mpsc::sync_channel(1);
    let build = std::thread::spawn(move || {
        let result = runtime.build(options).map(|_| ());
        let _ = sender.send(result);
    });
    let mut run = ProviderRun {
        fixture,
        control: None,
        build: Some(build),
    };
    let accept_started = Instant::now();
    let control = match run.fixture.accept_child() {
        Ok(control) => control,
        Err(error) => {
            let accept_wait_ms = accept_started.elapsed().as_millis();
            let provider_outcome = match receipt.try_lock().ok().and_then(|held| *held) {
                Some(InitialInspectionOutcomeForTest::CustodyUnavailable(reason)) => {
                    format!("CustodyUnavailable({reason:?})")
                }
                Some(InitialInspectionOutcomeForTest::Completed(success)) => {
                    format!("Completed({success})")
                }
                Some(InitialInspectionOutcomeForTest::NotStarted(reason)) => {
                    format!("NotStarted({reason:?})")
                }
                Some(InitialInspectionOutcomeForTest::ReapedFailure(kind)) => {
                    format!("ReapedFailure({kind:?})")
                }
                Some(InitialInspectionOutcomeForTest::RetainedFailure) => "RetainedFailure".into(),
                None => "Unknown".into(),
            };
            let build_result = match receiver.try_recv() {
                Ok(Ok(())) => "CompletedOk",
                Ok(Err(EngineError::Container(_))) => "CompletedErr(Container)",
                Ok(Err(EngineError::Config(_))) => "CompletedErr(Config)",
                Ok(Err(_)) => "CompletedErr(Other)",
                Err(mpsc::TryRecvError::Empty) => "Empty",
                Err(mpsc::TryRecvError::Disconnected) => "Disconnected",
            };
            let build_finished = run.build.as_ref().map(JoinHandle::is_finished);
            eprintln!(
                "initial inspection accept failure: provider_outcome={provider_outcome} build_result={build_result} build_finished={build_finished:?} accept_wait_ms={accept_wait_ms}"
            );
            return Err(error);
        }
    };
    run.control = Some(control);
    let pid = run
        .control
        .as_mut()
        .ok_or("provider child latch is unavailable")?
        .child_pid()?;
    let bounded_result = receiver.recv_timeout(response_bound);
    let reaped_before_release = process_is_reaped(pid);
    run.release_and_join()?;

    let result = bounded_result.map_err(|_| {
        format!(
            "public initial image inspection exceeded its {:?} observable bound",
            response_bound
        )
    })?;
    assert!(
        result.is_err(),
        "a killed initial image inspection must fail the public build"
    );
    assert!(
        reaped_before_release,
        "public build returned before the owned provider child was actually reaped"
    );
    Ok(())
}

fn exercise_public_absolute_inspection_deadline() -> Result<(), Box<dyn Error>> {
    use super::gated_launch::{ProviderCliReapedFailureKind, ProviderCliStartFailure};
    use super::runtime::InitialInspectionTerminalProofReceiptForTest;

    let gate_timeout = Duration::from_secs(1);
    let response_bound = gate_timeout + MAX_PROVIDER_CUSTODY_SHUTDOWN;
    let gate = GateFixture::new()?;
    let options = gate.resolved_options(gate_timeout)?;
    let fixture = ProviderFixture::new(FixtureMode::Block)?;
    let receipt: InitialInspectionTerminalProofReceiptForTest = Arc::new(Mutex::new(None));
    let mut runtime =
        ContainerRuntime::apple_with_provider_executable_for_test(&fixture.executable);
    runtime.set_initial_inspection_terminal_proof_for_test(Arc::clone(&receipt));
    let (start_tx, start_rx) = mpsc::sync_channel(1);
    let (sender, receiver) = mpsc::sync_channel(1);
    let build = std::thread::Builder::new().spawn(move || {
        if start_rx.recv().is_ok() {
            let result = runtime.build(options).map(|_| ());
            let _ = sender.send(result);
        }
    })?;
    let mut run = ProviderRun {
        fixture,
        control: None,
        build: Some(build),
    };
    let invoked = Instant::now();
    start_tx
        .send(())
        .map_err(|_| "public initial inspection worker did not receive invocation")?;
    let result = receiver
        .recv_timeout(response_bound.saturating_sub(invoked.elapsed()))
        .map_err(|_| "public initial inspection exceeded its original total response allowance")?;
    let response_elapsed = invoked.elapsed();
    let proof = receipt
        .try_lock()
        .map_err(|_| "public initial inspection terminal proof is unavailable")?
        .clone()
        .ok_or("public initial inspection did not record its terminal proof")?;

    assert!(response_elapsed <= response_bound);
    assert!(
        matches!(result, Err(EngineError::Container(_))),
        "deadline failure must preserve the public Container error"
    );
    match proof.outcome {
        InitialInspectionOutcomeForTest::NotStarted(ProviderCliStartFailure::DeadlineExpired) => {
            assert!(
                proof.reaped.is_none(),
                "a no-child deadline terminal cannot carry actual-child proof"
            );
        }
        InitialInspectionOutcomeForTest::ReapedFailure(
            ProviderCliReapedFailureKind::DeadlineExceeded,
        ) => {
            let failure = proof
                .reaped
                .ok_or("a started deadline failure omitted its actual wait proof")?;
            assert_eq!(failure.kind, ProviderCliReapedFailureKind::DeadlineExceeded);
            assert_ne!(failure.child_pid, 0);
            assert!(
                !failure.status.success(),
                "the unreleased provider child cannot complete successfully"
            );
            assert!(
                process_is_reaped(failure.child_pid),
                "public build returned before its actual Child PID was reaped"
            );
        }
        outcome => panic!("unexpected public initial inspection terminal: {outcome:?}"),
    }
    run.release_and_join()?;
    Ok(())
}

pub(super) struct StartedInspectionFixture {
    _fixture: ProviderFixture,
    control: Option<ChildLatch>,
    child: Option<std::process::Child>,
    pub(super) pid: u32,
}

impl StartedInspectionFixture {
    pub(super) fn new() -> Result<Self, Box<dyn Error>> {
        use std::process::{Command, Stdio};

        let fixture = ProviderFixture::new(FixtureMode::Block)?;
        let child = Command::new(&fixture.executable)
            .args(["image", "inspect", IMAGE])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let pid = child.id();
        let mut owned = Self {
            _fixture: fixture,
            control: None,
            child: Some(child),
            pid,
        };
        owned.control = Some(owned._fixture.accept_child()?);
        let handshake_pid = owned
            .control
            .as_mut()
            .ok_or("started fixture control is unavailable")?
            .child_pid()?;
        assert_eq!(handshake_pid, owned.pid);
        let child = owned
            .child
            .as_mut()
            .ok_or("started fixture Child is unavailable")?;
        assert!(
            child.try_wait()?.is_none(),
            "fixture exited before ownership transfer"
        );
        assert_eq!(
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(i32::try_from(owned.pid)?), None),
            Ok(()),
            "handshaken fixture must be live before deadline supervision"
        );
        Ok(owned)
    }

    pub(super) fn take_child(&mut self) -> Result<std::process::Child, Box<dyn Error>> {
        self.child
            .take()
            .ok_or_else(|| "started fixture Child was already transferred".into())
    }
}

impl Drop for StartedInspectionFixture {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            if !matches!(child.try_wait(), Ok(Some(_))) {
                if let Err(error) = child.kill() {
                    eprintln!("started inspection fixture cleanup kill failed: {error}");
                }
                if let Err(error) = child.wait() {
                    eprintln!("started inspection fixture cleanup wait failed: {error}");
                }
            }
        }
        self.control.take();
    }
}

#[test]
fn public_initial_image_inspection_absolute_deadline_preserves_terminal_custody(
) -> Result<(), Box<dyn Error>> {
    exercise_public_absolute_inspection_deadline()
}

#[test]
fn public_initial_image_inspection_stdout_cap_cancels_and_reaps_before_gate_deadline(
) -> Result<(), Box<dyn Error>> {
    exercise_public_initial_inspection(
        FixtureMode::StdoutOverflow,
        Duration::from_secs(30),
        MAX_PROVIDER_CUSTODY_SHUTDOWN,
    )
}

#[test]
#[ignore = "subprocess fixture invoked by initial image inspection tests"]
fn image_inspection_fixture_child() -> Result<(), Box<dyn Error>> {
    let Some(mode) = std::env::var_os(CHILD_MODE_ENV) else {
        return Ok(());
    };
    let socket = std::env::var_os(CHILD_SOCKET_ENV).ok_or("missing fixture socket")?;
    let mut control = UnixStream::connect(socket)?;
    control.set_read_timeout(Some(MAX_PROVIDER_CUSTODY_SHUTDOWN))?;
    control.set_write_timeout(Some(MAX_PROVIDER_CUSTODY_SHUTDOWN))?;
    control.write_all(&std::process::id().to_ne_bytes())?;
    control.flush()?;
    let mut handshake = [0_u8; 1];
    control.read_exact(&mut handshake)?;
    if handshake != [HANDSHAKE_ACK] {
        return Err("invalid provider fixture handshake acknowledgement".into());
    }
    control.set_read_timeout(None)?;
    let _overflow = if mode == "stdout-overflow" {
        let mut ready = control.try_clone()?;
        Some(
            std::thread::Builder::new()
                .name("initial-image-inspection-overflow".into())
                .spawn(move || {
                    if ready.write_all(&[CHILD_READY]).is_err() || ready.flush().is_err() {
                        return;
                    }
                    let _ = std::io::stdout().write_all(&vec![b'o'; MAX_PROVIDER_STDOUT + 1]);
                    let _ = std::io::stdout().flush();
                })?,
        )
    } else {
        control.write_all(&[CHILD_READY])?;
        control.flush()?;
        None
    };
    let mut release = [0_u8; 1];
    control.read_exact(&mut release)?;
    if release != [RELEASE] {
        return Err("invalid provider fixture release".into());
    }
    std::process::exit(0)
}
