//! `awman ready` exit status and verdict when a required step fails.
//!
//! The container runtime and the host agent are external boundaries reached by
//! bare name (`docker`, `claude`), so a stub directory earlier on `PATH` stands
//! in for both. No real container runtime or agent is needed.

use std::path::Path;
use std::process::{Command, Output, Stdio};

fn awman_bin() -> std::path::PathBuf {
    crate::awman_bin()
}

/// Initialise a minimal git repo at `path` with the agent Dockerfile and repo
/// Dockerfile already present, so `ready` neither prompts nor downloads.
fn ready_repo(path: &Path) {
    Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(path)
        .status()
        .expect("git init");
    let awman_dir = path.join(".awman");
    std::fs::create_dir_all(&awman_dir).expect("create .awman");
    std::fs::write(awman_dir.join("Dockerfile.claude"), "FROM scratch\n")
        .expect("agent dockerfile");
    std::fs::write(path.join("Dockerfile.dev"), "FROM scratch\n").expect("dockerfile");
}

/// A `PATH` value whose first entry stubs `docker` and `claude`.
///
/// The `docker` stub answers `info` and `image inspect` successfully and exits
/// `build_exit` for `build`. The `claude` stub answers the host ping.
#[cfg(unix)]
fn stub_path(build_exit: i32) -> (tempfile::TempDir, std::ffi::OsString) {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("shim dir");
    let docker = format!(
        "#!/bin/sh\ncase \"$1\" in\n  build) echo 'stub build failed' >&2; exit {build_exit} ;;\n  *) exit 0 ;;\nesac\n"
    );
    for (name, body) in [
        ("docker", docker.as_str()),
        ("claude", "#!/bin/sh\necho hello\n"),
    ] {
        let stub = dir.path().join(name);
        std::fs::write(&stub, body).expect("write stub");
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755))
            .expect("chmod stub");
    }
    let mut value = std::ffi::OsString::from(dir.path());
    if let Some(existing) = std::env::var_os("PATH") {
        value.push(":");
        value.push(existing);
    }
    (dir, value)
}

#[cfg(unix)]
fn run_ready(args: &[&str], build_exit: i32) -> Output {
    let repo = tempfile::tempdir().expect("repo dir");
    let home = tempfile::tempdir().expect("home dir");
    ready_repo(repo.path());
    let (_shims, path) = stub_path(build_exit);
    Command::new(awman_bin())
        .arg("ready")
        .args(args)
        .current_dir(repo.path())
        .env("AWMAN_CONFIG_HOME", home.path())
        .env("HOME", home.path())
        .env("PATH", &path)
        .stdin(Stdio::null())
        .output()
        .expect("awman ready")
}

fn combined(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[cfg(unix)]
#[test]
fn ready_build_with_failed_base_image_exits_non_zero_and_names_the_step() {
    let out = run_ready(&["--build", "--non-interactive"], 64);
    let text = combined(&out);
    assert!(
        !out.status.success(),
        "a Failed base image build must make `awman ready` exit non-zero; exit: {:?}; output: {text}",
        out.status.code()
    );
    assert!(
        text.contains("Base image:"),
        "the error must name the failed step; output: {text}"
    );
    assert!(
        text.contains("awman ready --build"),
        "the error must say to re-run `awman ready --build`; output: {text}"
    );
    assert!(
        !text.contains("awman is ready."),
        "a failed ready must not claim success; output: {text}"
    );
}

#[cfg(unix)]
#[test]
fn ready_json_with_failed_base_image_exits_zero_and_reports_not_ready() {
    let out = run_ready(&["--build", "--json"], 64);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "`ready --json` keeps exit 0 when a step fails; exit: {:?}; output: {}",
        out.status.code(),
        combined(&out)
    );
    let compact: String = stdout.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(
        compact.contains("\"ready\":false"),
        "`ready --json` must report \"ready\": false; stdout: {stdout}"
    );
}

#[cfg(unix)]
#[test]
fn ready_with_all_required_steps_done_exits_zero_and_reports_ready() {
    // No --build: the stub reports both images present, so nothing is built
    // and the audit is skipped (no --refresh).
    let out = run_ready(&["--non-interactive"], 64);
    let text = combined(&out);
    assert!(
        out.status.success(),
        "all required steps Done must exit 0; exit: {:?}; output: {text}",
        out.status.code()
    );
    assert!(
        text.contains("awman is ready."),
        "a successful ready must print the verdict; output: {text}"
    );
}
