//! WI 0118 Layer 1 startup-gate option and wrapper contract tests.

use awman::data::startup_gate::{
    load_startup_gate, StartupGateAccess, StartupGateBinding, StartupGateRequest, StartupGateSpec,
};
use awman::engine::container::options::{
    ContainerOption, EnvLiteral, EnvVar, ImageRef, OverlayPermission, OverlaySpec,
    ResolvedContainerOptions,
};
use awman::engine::container::startup_gate::{
    stage_startup_gate, wrap_entrypoint, CONTAINER_GATE_ROOT,
};
use sha2::{Digest, Sha256};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

fn valid_gate() -> (tempfile::TempDir, StartupGateSpec) {
    let canonical_temp = fs::canonicalize(std::env::temp_dir()).expect("canonical temp root");
    let control = tempfile::tempdir_in(canonical_temp).expect("control");
    fs::set_permissions(control.path(), fs::Permissions::from_mode(0o700)).expect("control mode");
    let manifest = br#"{"version":1,"entries":[]}"#;
    fs::write(control.path().join("review.manifest.json"), manifest).expect("manifest");
    fs::set_permissions(
        control.path().join("review.manifest.json"),
        fs::Permissions::from_mode(0o600),
    )
    .expect("manifest mode");
    let digest: String = Sha256::digest(manifest)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let request = serde_json::json!({"version":1,"bindings":[{"id":"review-input","workspace_path":"/review/input","manifest_id":digest,"manifest_file":"review.manifest.json","access":"read-only"}]});
    fs::write(
        control.path().join("request.json"),
        serde_json::to_vec(&request).expect("json"),
    )
    .expect("request");
    fs::set_permissions(
        control.path().join("request.json"),
        fs::Permissions::from_mode(0o600),
    )
    .expect("request mode");
    let spec = load_startup_gate(control.path(), Duration::from_secs(120)).expect("valid fixture");
    (control, spec)
}

fn gate_spec(control_dir: PathBuf) -> StartupGateSpec {
    StartupGateSpec {
        control_dir,
        request: StartupGateRequest {
            version: 1,
            bindings: vec![StartupGateBinding {
                id: "review-input".into(),
                workspace_path: "/review/input".into(),
                manifest_id: "0".repeat(64),
                manifest_file: "review.manifest.json".into(),
                access: StartupGateAccess::ReadOnly,
            }],
        },
        timeout: Duration::from_secs(120),
        validated_manifests: Default::default(),
    }
}

#[test]
fn stage_uses_only_fixed_operational_mounts_and_isolated_absolute_python() {
    let (control, spec) = valid_gate();
    let staged = stage_startup_gate(&spec).expect("stage");
    assert_eq!(staged.overlays.len(), 2);
    assert!(staged.overlays.iter().any(|overlay| {
        overlay.container_path.as_path() == std::path::Path::new("/.awman/startup-gate/bin")
            && overlay.permission == OverlayPermission::ReadOnly
    }));
    assert!(staged.overlays.iter().any(|overlay| {
        overlay.host_path == control.path()
            && overlay.container_path.as_path()
                == std::path::Path::new("/.awman/startup-gate/control")
            && overlay.permission == OverlayPermission::ReadWrite
    }));
    assert_eq!(
        &staged.wrapper_argv[..4],
        [
            "/usr/bin/python3",
            "-I",
            "-S",
            "/.awman/startup-gate/bin/bootstrap.py",
        ]
    );
    assert_eq!(CONTAINER_GATE_ROOT, "/.awman/startup-gate");
}

#[test]
fn staged_bootstrap_argv_contains_no_host_numeric_owner() {
    let (_control, spec) = valid_gate();
    let staged = stage_startup_gate(&spec).expect("stage");
    assert_eq!(
        staged.wrapper_argv,
        [
            "/usr/bin/python3",
            "-I",
            "-S",
            "/.awman/startup-gate/bin/bootstrap.py",
            "/.awman/startup-gate/control",
            "120",
        ]
    );
    assert!(
        !staged
            .wrapper_argv
            .iter()
            .any(|arg| arg == "--control-owner"),
        "a host UID/GID is not guest ownership authority"
    );
}

#[test]
fn wrapper_preserves_hostile_original_argv_as_exact_distinct_arguments() {
    let (_control, spec) = valid_gate();
    let staged = stage_startup_gate(&spec).expect("stage");
    let original = vec![
        "agent executable".to_string(),
        "--leading".to_string(),
        "space value".to_string(),
        "quote'\"value".to_string(),
        "line\nbreak".to_string(),
        "$(never-evaluate)".to_string(),
        "".to_string(),
    ];
    let wrapped = wrap_entrypoint(&staged, &original).expect("wrap");
    let separator = wrapped
        .iter()
        .position(|arg| arg == "--")
        .expect("argv boundary");
    assert_eq!(&wrapped[separator + 1..], original.as_slice());
}

#[test]
fn startup_gate_option_is_single_typed_value_and_absent_path_is_legacy_default() {
    let control = tempfile::tempdir().expect("control");
    let spec = gate_spec(control.path().to_path_buf());
    let gated = ResolvedContainerOptions::resolve([
        ContainerOption::Image(ImageRef::new("awman-claude:0.12.0")),
        ContainerOption::StartupGate(spec.clone()),
    ])
    .expect("one gate");
    let resolved_gate = gated.startup_gate.as_ref().expect("resolved gate");
    assert_eq!(resolved_gate.control_dir, spec.control_dir);
    assert_eq!(resolved_gate.request, spec.request);
    assert_eq!(resolved_gate.timeout, spec.timeout);

    let duplicate = ResolvedContainerOptions::resolve([
        ContainerOption::StartupGate(spec.clone()),
        ContainerOption::StartupGate(spec),
    ]);
    assert!(duplicate.is_err(), "duplicate startup gates must conflict");

    let legacy = ResolvedContainerOptions::resolve([ContainerOption::Image(ImageRef::new(
        "awman-claude:0.12.0",
    ))])
    .expect("legacy options");
    assert!(legacy.startup_gate.is_none());
}

#[test]
fn gated_resolution_rejects_reserved_overlay_overlap_and_loader_environment() {
    let forbidden_paths = [
        "/",
        "/bin",
        "/sbin/tool",
        "/usr/local",
        "/lib",
        "/lib64/x",
        "/etc/agent",
        "/proc",
        "/sys/x",
        "/dev",
        "/.awman/startup-gate/child",
    ];
    for path in forbidden_paths {
        let control = tempfile::tempdir().expect("control");
        let result = ResolvedContainerOptions::resolve([
            ContainerOption::StartupGate(gate_spec(control.path().to_path_buf())),
            ContainerOption::Overlay(OverlaySpec {
                host_path: PathBuf::from("/host/input"),
                container_path: PathBuf::from(path),
                permission: OverlayPermission::ReadOnly,
            }),
        ]);
        assert!(result.is_err(), "gated overlay {path} must be rejected");
    }

    for key in ["LD_PRELOAD", "LD_LIBRARY_PATH", "DYLD_INSERT_LIBRARIES"] {
        let control = tempfile::tempdir().expect("control");
        let result = ResolvedContainerOptions::resolve([
            ContainerOption::StartupGate(gate_spec(control.path().to_path_buf())),
            ContainerOption::EnvLiteral(EnvLiteral {
                key: key.into(),
                value: "/workspace/hostile.so".into(),
            }),
        ]);
        assert!(result.is_err(), "gated environment {key} must be rejected");

        let control = tempfile::tempdir().expect("control");
        let passthrough = ResolvedContainerOptions::resolve([
            ContainerOption::StartupGate(gate_spec(control.path().to_path_buf())),
            ContainerOption::EnvPassthrough(EnvVar(key.into())),
        ]);
        assert!(
            passthrough.is_err(),
            "gated passthrough environment {key} must be rejected even when unset"
        );
    }
}

#[test]
fn gate_binding_roots_are_allowlisted_and_never_accept_an_allowed_root_ancestor() {
    for accepted in [
        "/workspace",
        "/workspace/src",
        "/review/input",
        "/work/a",
        "/data/a",
        "/mnt/a",
        "/output/results",
    ] {
        let control = tempfile::tempdir().expect("control");
        let mut spec = gate_spec(control.path().to_path_buf());
        spec.request.bindings[0].workspace_path = accepted.into();
        assert!(
            ResolvedContainerOptions::resolve([ContainerOption::StartupGate(spec)]).is_ok(),
            "allowed source binding {accepted}"
        );
    }
    for rejected in [
        "/",
        "/workspace-parent",
        "/reviewed",
        "/var",
        "/home",
        "/usr",
    ] {
        let control = tempfile::tempdir().expect("control");
        let mut spec = gate_spec(control.path().to_path_buf());
        spec.request.bindings[0].workspace_path = rejected.into();
        assert!(
            ResolvedContainerOptions::resolve([ContainerOption::StartupGate(spec)]).is_err(),
            "source binding {rejected} must fail"
        );
    }
}

#[test]
fn stage_rejects_request_changed_after_load() {
    let (control, spec) = valid_gate();
    let mut changed: serde_json::Value =
        serde_json::from_slice(&fs::read(control.path().join("request.json")).expect("request"))
            .expect("json");
    changed["bindings"][0]["id"] = serde_json::Value::String("changed-input".into());
    fs::write(
        control.path().join("request.json"),
        serde_json::to_vec(&changed).expect("json"),
    )
    .expect("rewrite");
    fs::set_permissions(
        control.path().join("request.json"),
        fs::Permissions::from_mode(0o600),
    )
    .expect("request mode");
    assert!(stage_startup_gate(&spec).is_err());
}

#[test]
fn stage_rejects_manifest_changed_after_load() {
    let (control, spec) = valid_gate();
    fs::write(
        control.path().join("review.manifest.json"),
        br#"{"version":1,"entries":[{"path":"changed","kind":"directory","size":0,"sha256":null}]}"#,
    )
    .expect("replace caller manifest");
    fs::set_permissions(
        control.path().join("review.manifest.json"),
        fs::Permissions::from_mode(0o600),
    )
    .expect("manifest mode");
    assert!(
        stage_startup_gate(&spec).is_err(),
        "pre-stage changes must be rejected rather than copied"
    );
}

#[test]
fn staged_snapshot_is_immutable_after_caller_control_changes() {
    let (control, spec) = valid_gate();
    let approved_manifest = spec
        .validated_manifests
        .get("review.manifest.json")
        .expect("validated manifest")
        .clone();
    let staged = stage_startup_gate(&spec).expect("stage");
    let snapshot_root = staged
        .overlays
        .iter()
        .find(|overlay| {
            overlay.container_path.as_path() == std::path::Path::new("/.awman/startup-gate/bin")
        })
        .expect("snapshot overlay")
        .host_path
        .clone();
    let request_before = fs::read(snapshot_root.join("request.json")).expect("snapshot request");
    let manifest_before =
        fs::read(snapshot_root.join("review.manifest.json")).expect("snapshot manifest");
    assert_eq!(
        manifest_before, approved_manifest,
        "staging must write the bytes returned by bounded validation"
    );
    fs::write(control.path().join("request.json"), b"changed caller bytes")
        .expect("mutate request");
    fs::write(
        control.path().join("review.manifest.json"),
        b"changed caller bytes",
    )
    .expect("mutate manifest");
    assert_eq!(
        fs::read(snapshot_root.join("request.json")).expect("snapshot request"),
        request_before
    );
    assert_eq!(
        fs::read(snapshot_root.join("review.manifest.json")).expect("snapshot manifest"),
        manifest_before
    );
}
