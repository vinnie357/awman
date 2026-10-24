/// Integration tests that build each agent's Dockerfile template using Docker.
///
/// These tests verify that the template Dockerfiles produce valid images
/// in their default/template states. They require a running Docker daemon
/// and network access, and are skipped if Docker is unavailable.
use std::io::Write;
use std::process::Command;

fn docker_available() -> bool {
    Command::new("docker")
        .args(["info"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Build the project base image and return its tag. The base image is used to
/// substitute the `{{AWMAN_BASE_IMAGE}}` placeholder in agent templates.
fn build_base_image() -> String {
    let tag = "awman-test-base:latest";
    let status = Command::new("docker")
        .args([
            "build",
            "-t",
            tag,
            "-f",
            "templates/Dockerfile.project",
            ".",
        ])
        .status()
        .expect("failed to invoke docker build for project base image");
    assert!(
        status.success(),
        "docker build failed for project base template"
    );
    tag.to_string()
}

fn build_template(template_path: &str, tag: &str) {
    build_template_then(template_path, tag, |_| {});
}

/// Build a template, run `check` against the built image tag, then clean up.
fn build_template_then(template_path: &str, tag: &str, check: impl FnOnce(&str)) {
    if !docker_available() {
        eprintln!("Docker not available, skipping Dockerfile build test");
        return;
    }

    let base_tag = build_base_image();

    // Read the template and substitute the {{AWMAN_BASE_IMAGE}} placeholder so
    // Docker receives a valid image reference in the FROM directive.
    let template_content =
        std::fs::read_to_string(template_path).expect("failed to read template file");
    let dockerfile_content = template_content.replace("{{AWMAN_BASE_IMAGE}}", &base_tag);

    // Write the substituted content to a temporary file.
    let mut tmp = tempfile::NamedTempFile::new().expect("failed to create temp Dockerfile");
    tmp.write_all(dockerfile_content.as_bytes())
        .expect("failed to write temp Dockerfile");
    let tmp_path = tmp.path().to_path_buf();

    let status = Command::new("docker")
        .args([
            "build",
            "-t",
            tag,
            "-f",
            tmp_path.to_str().expect("temp path is not valid UTF-8"),
            ".",
        ])
        .status()
        .expect("failed to invoke docker build");

    assert!(
        status.success(),
        "docker build failed for template: {}",
        template_path
    );

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| check(tag)));

    // Clean up the test images.
    for image in &[tag, &base_tag as &str] {
        let _ = Command::new("docker")
            .args(["rmi", image])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

#[test]
fn build_claude_template() {
    build_template("templates/Dockerfile.claude", "awman-test-claude:latest");
}

#[test]
fn build_codex_template() {
    build_template_then(
        "templates/Dockerfile.codex",
        "awman-test-codex:latest",
        |tag| {
            // Codex resolves its package layout from the canonical exe path: the
            // exe dir must be `bin` with `../codex-package.json` beside it. Run as
            // the runtime user with an empty overlay on ~/.codex, as altana does.
            let home = tempfile::TempDir::new().expect("failed to create temp codex home");
            let mount = format!("{}:/home/awman/.codex", home.path().display());
            let output = Command::new("docker")
            .args([
                "run", "--rm", "--user", "awman", "-v", &mount, "--entrypoint", "sh", tag, "-c",
                r#"p="$(readlink -f "$(command -v codex)")"; [ "$(basename "$(dirname "$p")")" = bin ] && test -r "$(dirname "$p")/../codex-package.json""#,
            ])
            .output()
            .expect("failed to invoke docker run");
            assert!(
                output.status.success(),
                "codex package layout check failed in built image; stderr: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        },
    );
}

#[test]
fn build_opencode_template() {
    build_template(
        "templates/Dockerfile.opencode",
        "awman-test-opencode:latest",
    );
}
