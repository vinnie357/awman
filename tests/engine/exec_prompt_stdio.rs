//! Ordinary Claude exec uses the real CLI frontend and Apple spawn path.
//! Only the external container executable is replaced, inside a fresh child.

#![cfg(unix)]

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use awman::command::commands::agent_setup::HasAgentFrontend;
use awman::command::dispatch::catalogue::CommandCatalogue;
use awman::engine::container::options::ResolvedContainerOptions;
use awman::engine::container::{
    ContainerName, ContainerOption, ContainerRuntime, Entrypoint, ImageRef,
};
use awman::frontend::cli::CliFrontend;

const CHILD_MODE: &str = "AWMAN_EXEC_STDIO_FIXTURE_MODE";
const PROMPT: &str = "ordinary Claude prompt through stdin";
const STDOUT: &str = "ordinary-Claude-stdout";
const STDERR: &str = "ordinary-Claude-stderr";

fn fixture() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().expect("private fixture directory");
    let bin = root.path().join("bin");
    std::fs::create_dir(&bin).expect("fixture bin directory");
    let executable = bin.join("container");
    std::fs::write(
        &executable,
        r#"#!/bin/sh
set -eu
[ "$1" = run ] || exit 81
printf '%s\n' "$@" > "$HOME/argv"
if [ -t 0 ] && [ -t 1 ] && [ -t 2 ]; then
    printf tty > "$HOME/streams"
else
    printf piped > "$HOME/streams"
fi
if [ "$AWMAN_EXEC_STDIO_FIXTURE_MODE" = piped ]; then
    cat > "$HOME/prompt"
fi
printf 'ordinary-Claude-stdout\n'
printf 'ordinary-Claude-stderr\n' >&2
"#,
    )
    .expect("write container boundary fixture");
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))
        .expect("executable fixture permissions");
    root
}

async fn exercise_frontend(interactive: bool) {
    let mut args = vec![
        "awman",
        "exec",
        "prompt",
        PROMPT,
        "--agent",
        "claude",
        "--model",
        "gemma-4-E4B-it-MLX-8bit",
    ];
    if !interactive {
        args.push("--non-interactive");
    }
    let matches = CommandCatalogue::get()
        .build_clap_command()
        .try_get_matches_from(args)
        .expect("ordinary Claude exec arguments");
    let mut cli = CliFrontend::new(matches);
    let frontend = cli.container_frontend_for_pty();
    let options = ResolvedContainerOptions::resolve([
        ContainerOption::Image(ImageRef::new("fixture:claude")),
        ContainerOption::Name(ContainerName::new("awman-stdio-fixture")),
        ContainerOption::Entrypoint(Entrypoint::new([
            "claude",
            "--model",
            "gemma-4-E4B-it-MLX-8bit",
        ])),
        ContainerOption::Interactive(interactive),
        ContainerOption::SeededPrompt(PROMPT.into()),
    ])
    .expect("ordinary container options");
    let instance = ContainerRuntime::apple()
        .build(options)
        .expect("Apple container build");
    let mut execution = instance
        .run_with_frontend(frontend)
        .expect("ordinary Apple launch");
    let exit = tokio::time::timeout(Duration::from_secs(5), execution.wait())
        .await
        .expect("ordinary exec must finish without terminal input")
        .expect("ordinary Apple wait");
    assert_eq!(exit.exit_code, 0);
}

fn assert_launch(root: &Path, interactive: bool) {
    let argv = std::fs::read_to_string(root.join("argv")).expect("runtime was invoked");
    let args: Vec<_> = argv.lines().collect();
    assert_eq!(args.first(), Some(&"run"));
    assert!(
        args.contains(&"claude"),
        "Claude stays inside container argv"
    );
    assert!(args.contains(&"gemma-4-E4B-it-MLX-8bit"));
    assert_eq!(args.contains(&"-it"), interactive, "runtime argv: {argv}");
    assert_eq!(
        std::fs::read_to_string(root.join("streams")).expect("runtime stream observation"),
        if interactive { "tty" } else { "piped" },
        "the real spawn path must agree with the frontend mode"
    );
    if !interactive {
        assert!(args.contains(&"-i"), "seeded piped stdin must stay open");
        assert!(!args.contains(&"-t"));
        assert!(!args.contains(&PROMPT), "piped prompt belongs on stdin");
        assert_eq!(
            std::fs::read_to_string(root.join("prompt")).expect("seeded prompt and EOF"),
            format!("{PROMPT}\n")
        );
    }
}

#[tokio::test]
async fn non_interactive_claude_exec_apple_uses_pipes_and_closes_seeded_stdin() {
    if std::env::var(CHILD_MODE).as_deref() == Ok("piped") {
        exercise_frontend(false).await;
        return;
    }
    let root = fixture();
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "exec_prompt_stdio::non_interactive_claude_exec_apple_uses_pipes_and_closes_seeded_stdin",
                "--nocapture",
            ])
            .env_clear()
            .env("HOME", root.path())
            .env("PATH", format!("{}:/usr/bin:/bin", root.path().join("bin").display()))
            .env(CHILD_MODE, "piped")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("piped child must exit without any controlling terminal")
    .expect("launch fixture child");
    assert!(output.status.success(), "fixture child: {output:?}");
    assert_launch(root.path(), false);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stdout.contains(STDOUT), "stdout: {stdout}");
    assert!(stderr.contains(STDERR), "stderr: {stderr}");
    assert!(!stdout.contains(STDERR), "stderr must retain its own pipe");
    assert!(!stderr.contains("raw mode"), "no terminal setup: {stderr}");
}

#[tokio::test]
async fn interactive_claude_exec_apple_preserves_pty() {
    if std::env::var(CHILD_MODE).as_deref() == Ok("interactive") {
        exercise_frontend(true).await;
        return;
    }
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};

    let root = fixture();
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("interactive parent terminal");
    let mut command = CommandBuilder::new(std::env::current_exe().expect("test executable"));
    command.args([
        "--exact",
        "exec_prompt_stdio::interactive_claude_exec_apple_preserves_pty",
        "--nocapture",
    ]);
    command.env_clear();
    command.env("HOME", root.path());
    command.env(
        "PATH",
        format!("{}:/usr/bin:/bin", root.path().join("bin").display()),
    );
    command.env("TERM", "xterm");
    command.env(CHILD_MODE, "interactive");
    let mut child = pair
        .slave
        .spawn_command(command)
        .expect("interactive child");
    drop(pair.slave);
    let master = pair.master;
    // macOS holds the exiting child on its last slave close until the master is read.
    let mut reader = master
        .try_clone_reader()
        .expect("interactive terminal reader");
    let (drained_tx, drained_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut drained = Vec::new();
        let _ = std::io::Read::read_to_end(&mut reader, &mut drained);
        let _ = drained_tx.send(drained);
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut poll = tokio::time::interval(Duration::from_millis(20));
    let status = loop {
        if let Some(status) = child.try_wait().expect("interactive child status") {
            break status;
        }
        if tokio::time::Instant::now() >= deadline {
            drop(master);
            child.kill().expect("kill timed out fixture child");
            let _ = child.wait();
            panic!("interactive fixture child did not finish");
        }
        poll.tick().await;
    };
    drop(master);
    let drained = drained_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("interactive terminal output must reach EOF");
    let output = String::from_utf8_lossy(&drained);
    assert!(status.success(), "interactive child: {status:?}");
    assert_launch(root.path(), true);
    assert!(output.contains(STDOUT), "terminal output: {output}");
    assert!(output.contains(STDERR), "terminal output: {output}");
}
