use super::*;
use sha2::{Digest, Sha256};
use std::error::Error;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

const TOKEN: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const NAME: &str = "awman-altana-native-p2";
const MANIFEST_NAME: &str = "workspace.manifest.json";

struct GateFixture {
    _root: tempfile::TempDir,
    parent: PathBuf,
    guest: PathBuf,
    request_bytes: Vec<u8>,
    manifest_bytes: Vec<u8>,
}

impl GateFixture {
    fn orchestrated() -> Result<Self, Box<dyn Error>> {
        let root = tempfile::tempdir()?;
        let parent = root.path().join("control-parent");
        let guest = parent.join("guest-control");
        std::fs::create_dir(&parent)?;
        std::fs::create_dir(&guest)?;
        set_mode(&parent, 0o700)?;
        set_mode(&guest, 0o700)?;

        let manifest_bytes = br#"{"version":1,"entries":[]}"#.to_vec();
        let manifest_digest = lower_hex(&Sha256::digest(&manifest_bytes));
        write_private(&parent.join(MANIFEST_NAME), &manifest_bytes)?;
        let request_bytes = serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "bindings": [{
                "id": "workspace",
                "workspace_path": "/workspace",
                "manifest_id": manifest_digest,
                "manifest_file": MANIFEST_NAME,
                "access": "read-write"
            }]
        }))?;
        write_private(&parent.join("request.json"), &request_bytes)?;
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
            guest,
            request_bytes,
            manifest_bytes,
        })
    }

    fn load(&self) -> Result<StartupGateSpec, StartupGateError> {
        load_startup_gate(&self.parent, Duration::from_secs(30))
    }

    fn rewrite_intent(&self, bytes: &[u8]) -> Result<(), Box<dyn Error>> {
        write_private(&self.parent.join("launch-intent.json"), bytes)
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> std::io::Result<()> {
    Ok(())
}

fn load_error(fixture: &GateFixture, accepted: &str) -> StartupGateError {
    match fixture.load() {
        Ok(_) => panic!("{accepted}"),
        Err(error) => error,
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    set_mode(path, 0o600)
}

fn lower_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn assert_non_secret_error(error: &StartupGateError) {
    let rendered = format!("{error:?} {error}");
    assert!(!rendered.contains(TOKEN));
    assert!(!rendered.contains("launch_token"));
}

#[test]
#[cfg(unix)]
fn orchestrated_loader_derives_identity_and_redacts_secret_surfaces() -> Result<(), Box<dyn Error>>
{
    let fixture = GateFixture::orchestrated()?;
    let spec = fixture.load()?;
    let expected_request_digest: [u8; 32] = Sha256::digest(&fixture.request_bytes).into();
    assert_eq!(spec.request_digest, expected_request_digest);
    assert_eq!(
        spec.validated_manifests[MANIFEST_NAME],
        fixture.manifest_bytes
    );

    let expected_token_digest: [u8; 32] = {
        let mut digest = Sha256::new();
        digest.update(b"awman-launch-token-v1\0");
        digest.update([0x11; 32]);
        digest.finalize().into()
    };
    let identity = match &spec.control.0 {
        StartupGateControlLayoutInner::Orchestrated { identity, .. } => identity,
        StartupGateControlLayoutInner::Legacy { .. } => panic!("expected orchestrated layout"),
    };
    assert_eq!(identity.container_name.as_str(), NAME);
    assert_eq!(identity.token_digest, expected_token_digest);
    assert_eq!(identity.request_digest, expected_request_digest);

    let rendered = format!("{:?}", spec.control);
    assert!(!rendered.contains(TOKEN));
    assert!(!rendered.contains(&fixture.parent.display().to_string()));
    assert!(!rendered.contains(&lower_hex(&expected_token_digest)));
    Ok(())
}

#[test]
#[cfg(unix)]
fn intention_parser_rejects_every_noncanonical_or_overflow_shape_without_secret_diagnostics(
) -> Result<(), Box<dyn Error>> {
    let fixture = GateFixture::orchestrated()?;
    let cases: Vec<Vec<u8>> = vec![
        vec![b'x'; 513],
        b"\xef\xbb\xbf{\"version\":1}".to_vec(),
        b"{\"version\":1,\"container_name\":\"awman-altana-native-p2\",\"launch_token\":\"\\u0031\"}".to_vec(),
        b"{\"version\":1,\"container_name\":\"awman-altana-native-p2\",\"launch_token\":\"1111111111111111111111111111111111111111111111111111111111111111\"}\0".to_vec(),
        "{\"version\":1,\"container_name\":\"awman-altana-native-p2\",\"launch_token\":\"1111111111111111111111111111111111111111111111111111111111111111\",\"note\":\"é\"}".as_bytes().to_vec(),
        format!("{{\"version\":1,\"version\":1,\"container_name\":\"{NAME}\",\"launch_token\":\"{TOKEN}\"}}").into_bytes(),
        format!("{{\"version\":\"1\",\"container_name\":\"{NAME}\",\"launch_token\":\"{TOKEN}\"}}").into_bytes(),
        format!("{{\"version\":2,\"container_name\":\"{NAME}\",\"launch_token\":\"{TOKEN}\"}}").into_bytes(),
        format!("{{\"version\":1,\"container_name\":\"Awman-altana-native\",\"launch_token\":\"{TOKEN}\"}}").into_bytes(),
        format!("{{\"version\":1,\"container_name\":\"awman-altana-native-\",\"launch_token\":\"{TOKEN}\"}}").into_bytes(),
        format!("{{\"version\":1,\"container_name\":\"{NAME}\",\"launch_token\":\"{}\"}}", "A".repeat(64)).into_bytes(),
        format!("{{\"version\":1,\"container_name\":\"{NAME}\",\"launch_token\":\"{}\"}}", &TOKEN[..62]).into_bytes(),
        format!("{{\"version\":1,\"container_name\":\"{NAME}\",\"launch_token\":\"{TOKEN}\",\"unknown\":1}}").into_bytes(),
        format!("{{\"version\":1,\"container_name\":\"{NAME}\",\"launch_token\":\"{TOKEN}\"}} trailing").into_bytes(),
    ];
    for bytes in cases {
        fixture.rewrite_intent(&bytes)?;
        let error = load_error(&fixture, "noncanonical intention accepted");
        assert_non_secret_error(&error);
    }
    Ok(())
}

#[test]
#[cfg(unix)]
fn intention_reader_accepts_the_exact_512_byte_limit() -> Result<(), Box<dyn Error>> {
    let fixture = GateFixture::orchestrated()?;
    let mut bytes =
        format!("{{\"version\":1,\"container_name\":\"{NAME}\",\"launch_token\":\"{TOKEN}\"}}")
            .into_bytes();
    bytes.resize(512, b' ');
    fixture.rewrite_intent(&bytes)?;
    let spec = fixture.load()?;
    assert!(matches!(
        &spec.control.0,
        StartupGateControlLayoutInner::Orchestrated { .. }
    ));
    Ok(())
}

#[test]
#[cfg(unix)]
fn every_fixed_gate_name_is_reserved_from_manifest_claims() -> Result<(), Box<dyn Error>> {
    let fixture = GateFixture::orchestrated()?;
    for reserved in [
        "bootstrap.py",
        "original-python-env.json",
        "request.json",
        "ready.json",
        "failure.json",
        "release.json",
        ".released",
        "launch-intent.json",
        "launch-plan.json",
        "launch.json",
        "launch-failure.json",
        "guest-control",
    ] {
        assert!(
            is_reserved_gate_name(reserved),
            "unreserved fixed name: {reserved}"
        );
    }
    assert!(!is_reserved_gate_name(MANIFEST_NAME));
    assert!(!is_reserved_gate_name("user-data.json"));

    let claimed = "bootstrap.py";
    write_private(&fixture.parent.join(claimed), &fixture.manifest_bytes)?;
    let request = serde_json::to_vec(&serde_json::json!({
        "version": 1,
        "bindings": [{
            "id": "workspace",
            "workspace_path": "/workspace",
            "manifest_id": lower_hex(&Sha256::digest(&fixture.manifest_bytes)),
            "manifest_file": claimed,
            "access": "read-write"
        }]
    }))?;
    write_private(&fixture.parent.join("request.json"), &request)?;
    let error = load_error(
        &fixture,
        "digest-valid reserved manifest claim was accepted",
    );
    assert_non_secret_error(&error);
    Ok(())
}

#[test]
#[cfg(unix)]
fn guest_control_must_be_exact_private_directory_and_is_never_adopted() -> Result<(), Box<dyn Error>>
{
    let fixture = GateFixture::orchestrated()?;
    set_mode(&fixture.guest, 0o750)?;
    let broad = load_error(&fixture, "broad guest directory accepted");
    assert_non_secret_error(&broad);

    std::fs::remove_dir(&fixture.guest)?;
    let missing = load_error(&fixture, "missing guest directory accepted");
    assert_non_secret_error(&missing);

    let target = fixture.parent.join("other-private-dir");
    std::fs::create_dir(&target)?;
    set_mode(&target, 0o700)?;
    std::os::unix::fs::symlink(&target, &fixture.guest)?;
    let linked = load_error(&fixture, "guest symlink accepted");
    assert_non_secret_error(&linked);
    assert!(target.is_dir());
    Ok(())
}

#[test]
#[cfg(unix)]
fn missing_intention_preserves_legacy_layout() -> Result<(), Box<dyn Error>> {
    let fixture = GateFixture::orchestrated()?;
    std::fs::remove_file(fixture.parent.join("launch-intent.json"))?;
    std::fs::remove_dir(&fixture.guest)?;
    let spec = fixture.load()?;
    assert!(matches!(
        &spec.control.0,
        StartupGateControlLayoutInner::Legacy { .. }
    ));
    Ok(())
}
