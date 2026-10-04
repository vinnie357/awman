use super::*;
use crate::data::startup_gate::{load_startup_gate, StartupGateError, StartupGateSpec};
use crate::engine::container::{OverlayPermission, OverlaySpec};
use sha2::{Digest, Sha256};
use std::error::Error;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

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

#[test]
#[cfg(unix)]
fn orchestrated_stager_keeps_parent_and_secret_off_guest_surfaces() -> Result<(), Box<dyn Error>> {
    let fixture = GateFixture::orchestrated()?;
    let spec = fixture.load()?;
    let (_, identity) = spec
        .control
        .orchestrated_parts()
        .ok_or("expected orchestrated startup gate")?;
    let expected_token_digest = identity.token_digest;

    let staged = stage_startup_gate(&spec)?;
    assert_eq!(staged.overlays.len(), 1);
    assert_eq!(staged.overlays[0].permission, OverlayPermission::ReadOnly);
    assert_eq!(
        staged.overlays[0].container_path,
        PathBuf::from(format!("{CONTAINER_GATE_ROOT}/bin"))
    );
    assert_ne!(staged.overlays[0].host_path, fixture.parent);
    assert_ne!(staged.overlays[0].host_path, fixture.guest);

    let static_root = &staged.overlays[0].host_path;
    let staged_request = std::fs::read(static_root.join("request.json"))?;
    assert_eq!(staged_request, fixture.request_bytes);
    let staged_request_digest: [u8; 32] = Sha256::digest(&staged_request).into();
    assert_eq!(staged_request_digest, spec.request_digest);
    assert_eq!(
        std::fs::read(static_root.join(MANIFEST_NAME))?,
        fixture.manifest_bytes
    );
    for forbidden in [
        "launch-intent.json",
        "launch-plan.json",
        "launch.json",
        "launch-failure.json",
        "ready.json",
        "failure.json",
        "release.json",
        ".released",
    ] {
        assert!(!static_root.join(forbidden).exists());
    }
    for entry in std::fs::read_dir(static_root)? {
        let path = entry?.path();
        if path.is_file() {
            let bytes = std::fs::read(path)?;
            assert!(!bytes
                .windows(TOKEN.len())
                .any(|window| window == TOKEN.as_bytes()));
        }
    }

    let mount = staged
        .orchestrated_mount
        .as_ref()
        .ok_or("missing orchestrated mount")?;
    let overlay: OverlaySpec = mount.revalidated_overlay()?;
    assert_eq!(overlay.permission, OverlayPermission::ReadWrite);
    assert_eq!(overlay.host_path, std::fs::canonicalize(&fixture.guest)?);
    assert_eq!(
        overlay.container_path,
        PathBuf::from(format!("{CONTAINER_GATE_ROOT}/control"))
    );

    let staged_debug = format!("{staged:?}");
    let argv = staged.wrapper_argv.join("\0");
    for surface in [&staged_debug, &argv] {
        assert!(!surface.contains(TOKEN));
        assert!(!surface.contains(&fixture.parent.display().to_string()));
        assert!(!surface.contains(&lower_hex(&expected_token_digest)));
    }
    for forbidden in [
        "request.json",
        MANIFEST_NAME,
        "launch-intent.json",
        "launch-plan.json",
        "launch.json",
    ] {
        assert!(!fixture.guest.join(forbidden).exists());
    }
    Ok(())
}

#[test]
#[cfg(unix)]
fn missing_intention_preserves_legacy_single_directory_mount() -> Result<(), Box<dyn Error>> {
    let fixture = GateFixture::orchestrated()?;
    std::fs::remove_file(fixture.parent.join("launch-intent.json"))?;
    std::fs::remove_dir(&fixture.guest)?;
    let spec = fixture.load()?;
    assert!(spec.control.orchestrated_parts().is_none());
    let staged = stage_startup_gate(&spec)?;
    assert!(staged.orchestrated_mount.is_none());
    assert_eq!(staged.overlays.len(), 2);
    assert_eq!(staged.overlays[1].permission, OverlayPermission::ReadWrite);
    assert_eq!(
        staged.overlays[1].host_path,
        std::fs::canonicalize(&fixture.parent)?
    );
    Ok(())
}
