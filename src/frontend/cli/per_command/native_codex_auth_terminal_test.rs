//! Isolated controlling-terminal checks. No provider or real login is invoked.
use super::CliNativeAuthProxy;
use crate::command::commands::exec_prompt::ExecPromptCommandFrontend;
use crate::engine::agent_runtime::AgentFrontend;
use crate::frontend::cli::command_frontend::CliFrontend;
use std::fs::File;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};
use std::time::Duration;

const HELPER: &str = "frontend::cli::per_command::container_frontend_marker::native_codex_auth_terminal_test::auth_terminal_subprocess";
const OUT: &[u8] = b"SYNTHETIC_PRIVATE_AUTH_STDOUT\n";
const ERR: &[u8] = b"SYNTHETIC_PRIVATE_AUTH_STDERR\n";
const MAIN: &[u8] = b"{\"type\":\"native.main.unchanged\"}\n";

#[derive(serde::Deserialize)]
struct FixtureResult {
    returncode: i32,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    terminal: Vec<u8>,
    problem: Option<String>,
    phases: Vec<serde_json::Value>,
    child_pid: Option<u32>,
    reaped: bool,
}

fn isolated_child(mode: &str) -> io::Result<Command> {
    let mut command = Command::new("python3");
    command
        .args([
            "-c",
            include_str!("native_codex_auth_terminal_fixture.py"),
            std::env::current_exe()?
                .to_str()
                .ok_or_else(|| io::Error::other("non-UTF8 test binary path"))?,
            HELPER,
            mode,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    Ok(command)
}

fn fixture(mode: &str) -> io::Result<(std::process::Output, Vec<u8>)> {
    let wrapper = collect(isolated_child(mode)?)?;
    if !wrapper.status.success() {
        return Err(io::Error::other(format!(
            "UNEXPECTED_TEST_ISSUE: Python PTY fixture failed: {}",
            String::from_utf8_lossy(&wrapper.stderr)
        )));
    }
    let result: FixtureResult = serde_json::from_slice(&wrapper.stdout)?;
    if let Some(problem) = result.problem {
        return Err(io::Error::other(format!(
            "{problem}; child_pid={:?}; reaped={}; phases={:?}; child_stderr={}",
            result.child_pid,
            result.reaped,
            result.phases,
            String::from_utf8_lossy(&result.stderr)
        )));
    }
    if !result.reaped {
        return Err(io::Error::other(format!(
            "OWNED_CHILD_CLEANUP_UNRESOLVED: child_pid={:?}; phases={:?}",
            result.child_pid, result.phases
        )));
    }
    let raw = if result.returncode < 0 {
        -result.returncode
    } else {
        result.returncode << 8
    };
    Ok((
        std::process::Output {
            status: std::process::ExitStatus::from_raw(raw),
            stdout: result.stdout,
            stderr: result.stderr,
        },
        result.terminal,
    ))
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
        let mut out = Vec::new();
        let mut err = Vec::new();
        let collect_output = async {
            use tokio::io::AsyncReadExt;
            let (status, out_result, err_result) = tokio::join!(
                child.wait(),
                stdout.read_to_end(&mut out),
                stderr.read_to_end(&mut err)
            );
            out_result?;
            err_result?;
            status
        };
        match tokio::time::timeout(Duration::from_secs(15), collect_output).await {
            Ok(status) => Ok(std::process::Output { status: status?, stdout: out, stderr: err }),
            Err(_) => {
                let killed = child.start_kill();
                let waited = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("isolated auth terminal helper timed out; wrapper_kill={killed:?}; wrapper_wait={waited:?}; partial_stdout={}; partial_stderr={}", String::from_utf8_lossy(&out), String::from_utf8_lossy(&err)),
                ))
            }
        }
    })
}

fn helper_phase(name: &str) {
    eprintln!(
        "AUTH_TERMINAL_CHILD_PHASE {name} pid={}",
        std::process::id()
    );
}

#[test]
fn auth_terminal_subprocess() {
    let Ok(mode) = std::env::var("AWMAN_AUTH_TERMINAL_TEST_CHILD") else {
        return;
    };
    helper_phase("helper-entered");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("test runtime");
    if mode == "idle" {
        // Scope: idle writer shutdown with live senders, not full retained-reader recovery.
        let held = runtime.block_on(async {
            let matches = clap::Command::new("awman").get_matches_from(["awman"]);
            let mut frontend = CliFrontend::new(matches);
            let mut private = frontend
                .native_auth_frontend()
                .expect("isolated controlling terminal");
            let io = private.take_io();
            io.stdout
                .send(b"PRIVATE_IDLE_WRITER_STARTED\n".to_vec())
                .expect("idle writer start marker queued");
            (frontend, private, io)
        });
        let mut permission = String::new();
        std::io::stdin()
            .read_line(&mut permission)
            .expect("observed writer start handshake");
        assert_eq!(
            permission, "DROP\n",
            "writer start was not observed before runtime drop"
        );
        println!("PRIVATE_IDLE_RUNTIME_DROP_READY");
        std::io::stdout()
            .flush()
            .expect("idle runtime drop readiness flushed");
        drop(runtime);
        drop(held);
        std::process::exit(0);
    }
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
            let slave = File::options().write(true).custom_flags(libc::O_NOCTTY)
                .open(std::env::var("AWMAN_AUTH_TERMINAL_TEST_SLAVE").expect("private slave path"))
                .expect("open private PTY slave");
            let mut private = CliNativeAuthProxy::new(
                slave, std::sync::Arc::clone(&frontend.native_output_writers),
            ).expect("private PTY terminal validation");
            println!("PRIVATE_AUTH_SINK_READY");
            std::io::stdout().flush().expect("sink readiness flushed");
            let mut closed = String::new();
            std::io::stdin().read_line(&mut closed).expect("private master close handshake");
            assert_eq!(closed, "CLOSED\n", "private master was not closed");
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
            helper_phase("private-channel-acquired");
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
            helper_phase("auth-output-drain-start");
            frontend
                .finish_native_output()
                .await
                .expect("actual auth writers drained");
            helper_phase("auth-output-drain-complete");
            assert!(frontend
                .native_output_writers
                .lock()
                .expect("writers")
                .is_empty());
            helper_phase("main-output-start");
            let mut main = frontend.native_container_frontend();
            let io = main.take_io();
            io.stdout.send(MAIN.to_vec()).expect("native main stdout");
            drop(io);
            drop(main);
            helper_phase("main-output-drain-start");
            frontend
                .finish_native_output()
                .await
                .expect("actual native main writer drained");
            helper_phase("main-output-drain-complete");
        }
    });
    helper_phase("helper-exit");
    std::process::exit(0);
}

#[test]
fn auth_terminal_cli_without_controlling_terminal_rejects_without_generic_fallback(
) -> io::Result<()> {
    let (output, _terminal) = fixture("absent")?;
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
    let (output, terminal) = fixture("tty")?;
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
    let (output, _terminal) = fixture("broken")?;
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

#[test]
fn auth_terminal_idle_writer_does_not_block_runtime_shutdown_with_live_senders() -> io::Result<()> {
    let (output, _terminal) = fixture("idle")?;
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("PRIVATE_IDLE_RUNTIME_DROP_READY"),
        "idle writer runtime drop boundary not reached"
    );
    assert!(
        output.status.success(),
        "idle writer blocked runtime shutdown or failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}
