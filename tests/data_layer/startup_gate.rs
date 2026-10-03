//! WI 0118 Layer 0 contract tests. Hermetic: no runtime, network, or model.

#![cfg(unix)]

use awman::data::startup_gate::{load_startup_gate, StartupGateAccess};
use sha2::{Digest, Sha256};
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

struct GateFixture {
    _tmp: tempfile::TempDir,
    control: PathBuf,
}

impl GateFixture {
    fn valid() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let control = tmp.path().join("control");
        fs::create_dir(&control).expect("control dir");
        fs::set_permissions(&control, fs::Permissions::from_mode(0o700)).expect("control mode");
        let manifest = br#"{"version":1,"entries":[{"path":"README.md","kind":"file","size":3,"sha256":"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"},{"path":"src","kind":"directory","size":0,"sha256":null}]}"#;
        fs::write(control.join("review.manifest.json"), manifest).expect("manifest");
        fs::set_permissions(
            control.join("review.manifest.json"),
            fs::Permissions::from_mode(0o600),
        )
        .expect("manifest mode");
        let request = serde_json::json!({
            "version": 1,
            "bindings": [{
                "id": "review-input",
                "workspace_path": "/review/input",
                "manifest_id": sha256_hex(manifest),
                "manifest_file": "review.manifest.json",
                "access": "read-only"
            }]
        });
        fs::write(
            control.join("request.json"),
            serde_json::to_vec(&request).expect("request json"),
        )
        .expect("request");
        fs::set_permissions(
            control.join("request.json"),
            fs::Permissions::from_mode(0o600),
        )
        .expect("request mode");
        Self { _tmp: tmp, control }
    }

    fn rewrite_request(&self, value: serde_json::Value) {
        fs::write(
            self.control.join("request.json"),
            serde_json::to_vec(&value).expect("request json"),
        )
        .expect("rewrite request");
        fs::set_permissions(
            self.control.join("request.json"),
            fs::Permissions::from_mode(0o600),
        )
        .expect("request mode");
    }

    fn request_value(&self) -> serde_json::Value {
        serde_json::from_slice(&fs::read(self.control.join("request.json")).expect("request"))
            .expect("request value")
    }

    fn load(
        &self,
    ) -> Result<
        awman::data::startup_gate::StartupGateSpec,
        awman::data::startup_gate::StartupGateError,
    > {
        load_startup_gate(&self.control, Duration::from_secs(120))
    }
}

fn assert_rejected(control: &Path, label: &str) {
    assert!(
        load_startup_gate(control, Duration::from_secs(120)).is_err(),
        "{label} must fail closed"
    );
}

#[test]
fn valid_v1_request_and_raw_manifest_digest_load_exactly() {
    let fixture = GateFixture::valid();
    let spec = fixture.load().expect("valid gate");
    assert_eq!(spec.timeout, Duration::from_secs(120));
    assert_eq!(spec.request.version, 1);
    assert_eq!(spec.request.bindings.len(), 1);
    let binding = &spec.request.bindings[0];
    assert_eq!(binding.id, "review-input");
    assert_eq!(binding.workspace_path, "/review/input");
    assert_eq!(binding.access, StartupGateAccess::ReadOnly);

    let raw = fs::read(fixture.control.join("review.manifest.json")).expect("raw manifest");
    assert_eq!(binding.manifest_id, sha256_hex(&raw));
    let mut changed_wire = raw.clone();
    changed_wire.push(b'\n');
    fs::write(fixture.control.join("review.manifest.json"), changed_wire).expect("changed wire");
    assert_rejected(&fixture.control, "raw-byte digest mismatch");
}

#[test]
fn request_schema_version_size_unknown_fields_and_timeout_are_strict() {
    let cases = [
        (
            "wrong version",
            serde_json::json!({"version":2,"bindings":[]}),
        ),
        (
            "unknown field",
            serde_json::json!({"version":1,"bindings":[],"extra":true}),
        ),
        ("missing bindings", serde_json::json!({"version":1})),
    ];
    for (label, request) in cases {
        let fixture = GateFixture::valid();
        fixture.rewrite_request(request);
        assert_rejected(&fixture.control, label);
    }

    let fixture = GateFixture::valid();
    let mut oversized = fs::read(fixture.control.join("request.json")).expect("valid request");
    oversized.resize(4097, b' ');
    fs::write(fixture.control.join("request.json"), oversized).expect("oversize");
    assert_rejected(&fixture.control, "oversized request");

    let zero = GateFixture::valid();
    assert!(load_startup_gate(&zero.control, Duration::ZERO).is_err());
    let excessive = GateFixture::valid();
    assert!(load_startup_gate(&excessive.control, Duration::from_secs(3601)).is_err());
}

#[test]
fn binding_ids_paths_manifests_and_access_are_strict() {
    let mutations = [
        ("id", serde_json::json!("../bad")),
        ("workspace_path", serde_json::json!("review/input")),
        ("workspace_path", serde_json::json!("/review/../etc")),
        ("workspace_path", serde_json::json!("/usr/local/work")),
        ("manifest_id", serde_json::json!("A".repeat(64))),
        ("manifest_file", serde_json::json!("../manifest.json")),
        ("access", serde_json::json!("write")),
    ];
    for (field, value) in mutations {
        let fixture = GateFixture::valid();
        let mut binding = fixture.request_value()["bindings"][0].clone();
        binding[field] = value;
        fixture.rewrite_request(serde_json::json!({"version":1,"bindings":[binding]}));
        assert_rejected(&fixture.control, field);
    }

    let fixture = GateFixture::valid();
    let first = fixture.request_value()["bindings"][0].clone();
    let mut duplicate_id = first.clone();
    duplicate_id["workspace_path"] = serde_json::json!("/review/second");
    fixture.rewrite_request(serde_json::json!({"version":1,"bindings":[first, duplicate_id]}));
    assert_rejected(&fixture.control, "duplicate binding id");

    let fixture = GateFixture::valid();
    let first = fixture.request_value()["bindings"][0].clone();
    let mut duplicate_path = first.clone();
    duplicate_path["id"] = serde_json::json!("second");
    fixture.rewrite_request(serde_json::json!({"version":1,"bindings":[first, duplicate_path]}));
    assert_rejected(&fixture.control, "duplicate binding path");

    let fixture = GateFixture::valid();
    let mut first = fixture.request_value()["bindings"][0].clone();
    first["workspace_path"] = serde_json::json!("/review/input");
    let mut child = first.clone();
    child["id"] = serde_json::json!("child");
    child["workspace_path"] = serde_json::json!("/review/input/src");
    fixture.rewrite_request(serde_json::json!({"version":1,"bindings":[first,child]}));
    assert_rejected(&fixture.control, "overlapping binding paths");
}

#[test]
fn manifest_wire_format_sorting_paths_kinds_and_size_are_strict() {
    let invalid = [
        br#"{"version":1,"entries":[],"extra":true}"#.to_vec(),
        br#"{"version":2,"entries":[]}"#.to_vec(),
        br#"{"version":1,"entries":[{"path":"b","kind":"file","size":0,"sha256":"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"},{"path":"a","kind":"directory","size":0,"sha256":null}]}"#.to_vec(),
        br#"{"version":1,"entries":[{"path":"a","kind":"directory","size":0,"sha256":null},{"path":"a","kind":"directory","size":0,"sha256":null}]}"#.to_vec(),
        br#"{"version":1,"entries":[{"path":"../a","kind":"directory","size":0,"sha256":null}]}"#.to_vec(),
        br#"{"version":1,"entries":[{"path":"a\\b","kind":"directory","size":0,"sha256":null}]}"#.to_vec(),
        br#"{"version":1,"entries":[{"path":"a","kind":"directory","size":1,"sha256":null}]}"#.to_vec(),
        br#"{"version":1,"entries":[{"path":"a","kind":"file","size":0,"sha256":null}]}"#.to_vec(),
    ];
    for manifest in invalid {
        let fixture = GateFixture::valid();
        fs::write(fixture.control.join("review.manifest.json"), &manifest).expect("manifest");
        let mut request = fixture.request_value();
        request["bindings"][0]["manifest_id"] = serde_json::json!(sha256_hex(&manifest));
        fixture.rewrite_request(request);
        assert_rejected(&fixture.control, "invalid manifest");
    }

    let fixture = GateFixture::valid();
    let mut oversized = br#"{"version":1,"entries":[]}"#.to_vec();
    oversized.resize(8 * 1024 * 1024 + 1, b' ');
    fs::write(fixture.control.join("review.manifest.json"), &oversized)
        .expect("oversized manifest");
    let mut request = fixture.request_value();
    request["bindings"][0]["manifest_id"] = serde_json::json!(sha256_hex(&oversized));
    fixture.rewrite_request(request);
    assert_rejected(&fixture.control, "oversized manifest");
}

#[test]
fn modes_symlinks_hardlinks_and_stale_status_files_fail_before_launch() {
    let fixture = GateFixture::valid();
    fs::set_permissions(&fixture.control, fs::Permissions::from_mode(0o755)).expect("mode");
    assert_rejected(&fixture.control, "control mode");

    let fixture = GateFixture::valid();
    fs::set_permissions(
        fixture.control.join("request.json"),
        fs::Permissions::from_mode(0o644),
    )
    .expect("mode");
    assert_rejected(&fixture.control, "request mode");

    let fixture = GateFixture::valid();
    let real = fixture.control.join("real-request.json");
    fs::rename(fixture.control.join("request.json"), &real).expect("rename");
    symlink(&real, fixture.control.join("request.json")).expect("symlink");
    assert_rejected(&fixture.control, "request symlink");

    let fixture = GateFixture::valid();
    fs::hard_link(
        fixture.control.join("review.manifest.json"),
        fixture.control.join("manifest-alias.json"),
    )
    .expect("hard link");
    assert_rejected(&fixture.control, "manifest hard link");

    for stale in ["ready.json", "release.json", "failure.json"] {
        let fixture = GateFixture::valid();
        fs::write(fixture.control.join(stale), b"{}").expect("stale status");
        assert_rejected(&fixture.control, stale);
    }
}
