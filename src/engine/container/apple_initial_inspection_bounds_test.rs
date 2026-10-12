use super::gated_launch::{MAX_PROVIDER_CUSTODY_SHUTDOWN, MAX_PROVIDER_STDOUT};
use super::{ContainerName, ContainerOption, ContainerRuntime, ImageRef};
use crate::data::startup_gate::load_startup_gate;
use crate::engine::container::options::ResolvedContainerOptions;
use sha2::{Digest, Sha256};
use std::error::Error;
use std::fs::OpenOptions;
use std::io::{Read as _, Write as _};
use std::net::Shutdown;
use std::os::fd::AsFd as _;
use std::os::unix::fs::{symlink, OpenOptionsExt as _, PermissionsExt as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
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
    let runtime = ContainerRuntime::apple_with_provider_executable_for_test(&fixture.executable);
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
    let control = run.fixture.accept_child()?;
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

#[test]
fn public_initial_image_inspection_deadline_cancels_and_reaps_before_latch_release(
) -> Result<(), Box<dyn Error>> {
    let gate_timeout = Duration::from_secs(1);
    exercise_public_initial_inspection(
        FixtureMode::Block,
        gate_timeout,
        gate_timeout + MAX_PROVIDER_CUSTODY_SHUTDOWN,
    )
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
