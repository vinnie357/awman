//! Isolated controlling-terminal checks. No provider or real login is invoked.
use super::CliNativeAuthProxy;
use crate::command::commands::exec_prompt::ExecPromptCommandFrontend;
use crate::engine::agent_runtime::AgentFrontend;
use crate::frontend::cli::command_frontend::CliFrontend;
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::Duration;

const HELPER: &str = "frontend::cli::per_command::container_frontend_marker::native_codex_auth_terminal_test::auth_terminal_subprocess";
const OUT: &[u8] = b"SYNTHETIC_PRIVATE_AUTH_STDOUT\n";
const ERR: &[u8] = b"SYNTHETIC_PRIVATE_AUTH_STDERR\n";
const MAIN: &[u8] = b"{\"type\":\"native.main.unchanged\"}\n";

fn isolated_child(slave: Option<&File>) -> io::Result<Command> {
    isolated_child_mode(slave, if slave.is_some() { "tty" } else { "absent" })
}

fn isolated_child_mode(slave: Option<&File>, mode: &str) -> io::Result<Command> {
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args(["--exact", HELPER, "--nocapture", "--quiet"])
        .env_clear()
        .env("AWMAN_AUTH_TERMINAL_TEST_CHILD", mode)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let slave_fd = slave.map(AsRawFd::as_raw_fd);
    // Only async-signal-safe libc calls execute between fork and exec.
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            if let Some(fd) = slave_fd {
                if libc::ioctl(fd, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    Ok(command)
}

fn collect(command: Command) -> io::Result<std::process::Output> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let mut command = tokio::process::Command::from(command);
        let mut child = command.kill_on_drop(true).spawn()?;
        let mut stdout = child.stdout.take().expect("captured child stdout");
        let mut stderr = child.stderr.take().expect("captured child stderr");
        let collect_output = async {
            use tokio::io::AsyncReadExt;
            let mut out = Vec::new();
            let mut err = Vec::new();
            let (status, out_result, err_result) = tokio::join!(
                child.wait(),
                stdout.read_to_end(&mut out),
                stderr.read_to_end(&mut err)
            );
            out_result?;
            err_result?;
            Ok(std::process::Output {
                status: status?,
                stdout: out,
                stderr: err,
            })
        };
        match tokio::time::timeout(Duration::from_secs(15), collect_output).await {
            Ok(result) => result,
            Err(_) => {
                child.kill().await?;
                child.wait().await?;
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "isolated auth terminal helper timed out",
                ))
            }
        }
    })
}

#[test]
fn auth_terminal_subprocess() {
    let Ok(mode) = std::env::var("AWMAN_AUTH_TERMINAL_TEST_CHILD") else {
        return;
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("test runtime");
    runtime.block_on(async {
        let matches = clap::Command::new("awman").get_matches_from(["awman"]);
        let mut frontend = CliFrontend::new(matches);
        if mode == "absent" {
            let result = frontend.native_auth_frontend();
            let error = match result {
                Err(error) => error.to_string(),
                Ok(_) => panic!("CLI without controlling terminal accepted native authentication"),
            };
            assert!(
                error.contains("controlling terminal") && error.contains("writable"),
                "actionable private-channel error required: {error}"
            );
            assert!(frontend
                .native_output_writers
                .lock()
                .expect("writers")
                .is_empty());
        } else if mode == "broken" {
            let mut master = -1;
            let mut slave = -1;
            let opened = unsafe {
                libc::openpty(
                    &mut master, &mut slave, std::ptr::null_mut(),
                    std::ptr::null(), std::ptr::null(),
                )
            };
            assert_eq!(opened, 0, "UNEXPECTED_TEST_ISSUE: private PTY creation failed");
            let master = unsafe { File::from_raw_fd(master) };
            let slave = unsafe { File::from_raw_fd(slave) };
            let mut private = CliNativeAuthProxy::new(
                slave, std::sync::Arc::clone(&frontend.native_output_writers),
            ).expect("private PTY terminal validation");
            drop(master);
            let io = private.take_io();
            io.stdout.send(OUT.to_vec())
                .expect("first public auth output queued");
            let _stderr_delivery = io.stderr.send(ERR.to_vec());
            drop(io);
            drop(private);
            assert!(
                frontend.finish_native_output().await.is_err(),
                "UNEXPECTED_TEST_ISSUE: closing private PTY master did not surface a real terminal write error"
            );
            assert!(
                frontend.native_output_writers.lock().expect("writers").is_empty(),
                "failed real writers were not drained"
            );
        } else {
            let mut private = frontend
                .native_auth_frontend()
                .expect("isolated controlling terminal");
            let mut io = private.take_io();
            assert!(io.resize.is_none());
            assert!(io.initial_size.is_none());
            assert!(
                matches!(
                    io.stdin_rx.try_recv(),
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty
                        | tokio::sync::mpsc::error::TryRecvError::Disconnected)
                ),
                "authentication received seeded input"
            );
            io.stdout.send(OUT.to_vec()).expect("auth stdout");
            io.stderr.send(ERR.to_vec()).expect("auth stderr");
            drop(io);
            drop(private);
            frontend
                .finish_native_output()
                .await
                .expect("actual auth writers drained");
            assert!(frontend
                .native_output_writers
                .lock()
                .expect("writers")
                .is_empty());
            let mut main = frontend.native_container_frontend();
            let io = main.take_io();
            io.stdout.send(MAIN.to_vec()).expect("native main stdout");
            drop(io);
            drop(main);
            frontend
                .finish_native_output()
                .await
                .expect("actual native main writer drained");
        }
    });
    std::process::exit(0);
}

#[test]
fn auth_terminal_cli_without_controlling_terminal_rejects_without_generic_fallback(
) -> io::Result<()> {
    let output = collect(isolated_child(None)?)?;
    assert!(
        output.status.success(),
        "isolated no-tty helper failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    for marker in [OUT, ERR] {
        assert!(!output
            .stdout
            .windows(marker.len())
            .any(|part| part == marker));
        assert!(!output
            .stderr
            .windows(marker.len())
            .any(|part| part == marker));
    }
    Ok(())
}

#[test]
fn auth_terminal_cli_routes_both_auth_streams_only_to_held_controlling_terminal() -> io::Result<()>
{
    let mut master = -1;
    let mut slave = -1;
    // The child obtains only this test's PTY; the runner's terminal stays untouched.
    if unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    let mut master = unsafe { File::from_raw_fd(master) };
    let slave = unsafe { File::from_raw_fd(slave) };
    let output = collect(isolated_child(Some(&slave))?)?;
    assert!(
        output.status.success(),
        "isolated PTY helper failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stdout.ends_with(MAIN),
        "native main bytes changed: {:?}",
        output.stdout
    );
    for marker in [OUT, ERR] {
        assert!(!output
            .stdout
            .windows(marker.len())
            .any(|part| part == marker));
        assert!(!output
            .stderr
            .windows(marker.len())
            .any(|part| part == marker));
    }
    // Keep the slave open and use nonblocking reads: empty output cannot masquerade as EOF.
    let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        || unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        return Err(io::Error::last_os_error());
    }
    let mut terminal = Vec::new();
    match master.read_to_end(&mut terminal) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
        Err(error) => return Err(error),
    }
    let terminal = String::from_utf8_lossy(&terminal);
    for marker in [
        "SYNTHETIC_PRIVATE_AUTH_STDOUT",
        "SYNTHETIC_PRIVATE_AUTH_STDERR",
    ] {
        assert!(terminal.contains(marker), "held terminal lost {marker}");
    }
    assert!(
        !terminal.contains("native.main.unchanged"),
        "main was redirected to authentication terminal"
    );
    Ok(())
}

#[test]
fn auth_terminal_constructor_rejects_nonterminal_file_before_registering_writers() -> io::Result<()>
{
    let file = tempfile::tempfile()?;
    let writers: crate::frontend::cli::command_frontend::NativeOutputWriters = Default::default();
    let result = CliNativeAuthProxy::new(file, std::sync::Arc::clone(&writers));
    assert!(
        result.is_err(),
        "ordinary file accepted as authentication terminal"
    );
    assert!(writers.lock().expect("writers").is_empty());
    Ok(())
}

#[test]
fn auth_terminal_cli_real_writer_failure_propagates_without_ordinary_output_fallback(
) -> io::Result<()> {
    let output = collect(isolated_child_mode(None, "broken")?)?;
    assert!(
        output.status.success(),
        "UNEXPECTED_TEST_ISSUE: isolated closed-master PTY helper failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    for marker in [OUT, ERR] {
        assert!(
            !output
                .stdout
                .windows(marker.len())
                .any(|part| part == marker),
            "failed private writer fell back to ordinary stdout"
        );
        assert!(
            !output
                .stderr
                .windows(marker.len())
                .any(|part| part == marker),
            "failed private writer fell back to ordinary stderr"
        );
    }
    Ok(())
}
