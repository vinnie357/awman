//! WI 0118 Layer 1 startup-gate option and wrapper contract tests.

use awman::data::startup_gate::{
    StartupGateAccess, StartupGateBinding, StartupGateRequest, StartupGateSpec,
};
use awman::engine::container::options::{
    ContainerOption, EnvLiteral, EnvVar, ImageRef, OverlayPermission, OverlaySpec,
    ResolvedContainerOptions,
};
use awman::engine::container::startup_gate::{
    stage_startup_gate, wrap_entrypoint, CONTAINER_GATE_ROOT,
};
use std::path::PathBuf;
use std::time::Duration;

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
    }
}

#[test]
fn stage_uses_only_fixed_operational_mounts_and_isolated_absolute_python() {
    let control = tempfile::tempdir().expect("control");
    let staged = stage_startup_gate(&gate_spec(control.path().to_path_buf())).expect("stage");
    assert_eq!(staged.overlays.len(), 2);
    assert!(staged.overlays.iter().any(|overlay| {
        overlay.container_path == PathBuf::from("/.awman/startup-gate/bin")
            && overlay.permission == OverlayPermission::ReadOnly
    }));
    assert!(staged.overlays.iter().any(|overlay| {
        overlay.host_path == control.path()
            && overlay.container_path == PathBuf::from("/.awman/startup-gate/control")
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
fn wrapper_preserves_hostile_original_argv_as_exact_distinct_arguments() {
    let control = tempfile::tempdir().expect("control");
    let staged = stage_startup_gate(&gate_spec(control.path().to_path_buf())).expect("stage");
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
