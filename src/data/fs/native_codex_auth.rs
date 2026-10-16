//! Ephemeral auth controls. Persistent owner evidence stays outside guest mounts.
use crate::data::error::DataError;
use crate::data::startup_gate::{
    load_startup_gate, OrchestratedControlAuthority, PlanFileError, StartupGateSpec,
};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

pub(crate) const GUEST_CODEX_HOME: &str = "/data/awman-native-sdk/codex";
pub(crate) const FRESH_CONFIG: &[u8] = b"cli_auth_credentials_store = \"file\"\n";
pub(crate) struct CodexAuthLaunchFiles {
    _directory: tempfile::TempDir,
    pub(crate) gate: StartupGateSpec,
    release: Option<AuthReleaseDestination>,
}

// A unique destination from actual private auth control creation, not from an
// arbitrary caller-supplied control authority. Permission stays in the engine.
pub(crate) struct AuthReleaseDestination {
    controls: Arc<OrchestratedControlAuthority>,
}
impl AuthReleaseDestination {
    pub(crate) fn matches_controls(&self, controls: &Arc<OrchestratedControlAuthority>) -> bool {
        Arc::ptr_eq(&self.controls, controls)
    }
    pub(crate) fn publish(self, nonce: &str) -> Result<(), PlanFileError> {
        #[cfg(unix)]
        {
            self.controls.publish_auth_release(nonce)
        }
        #[cfg(not(unix))]
        {
            let _ = nonce;
            Err(PlanFileError::PublicationFailed)
        }
    }
}
impl CodexAuthLaunchFiles {
    pub(crate) fn take_release_destination(&mut self) -> Option<AuthReleaseDestination> {
        self.release.take()
    }
    #[cfg(all(test, unix))]
    pub(crate) fn controls_parent_for_test(&self) -> &Path {
        self._directory.path()
    }

    pub(crate) fn new(data_root: &Path) -> Result<Self, DataError> {
        let mut builder = tempfile::Builder::new();
        builder.prefix("native-codex-auth-");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            builder.permissions(std::fs::Permissions::from_mode(0o700));
        }
        let directory = builder
            .tempdir_in(data_root)
            .map_err(|error| DataError::io(data_root, error))?;
        let guest = directory.path().join("guest-control");
        std::fs::create_dir(&guest).map_err(|error| DataError::io(&guest, error))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&guest, std::fs::Permissions::from_mode(0o700))
                .map_err(|error| DataError::io(&guest, error))?;
        }
        let config_hash = Sha256::digest(FRESH_CONFIG)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let manifest = serde_json::to_vec(&serde_json::json!({"version":1,"entries":[{
            "path":"config.toml","kind":"file","size":FRESH_CONFIG.len(),"sha256":config_hash
        }]}))
        .map_err(|error| DataError::Other(error.to_string()))?;
        let digest = Sha256::digest(&manifest)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        write_exclusive(directory.path(), "sdk.manifest.json", &manifest)?;
        let request = serde_json::to_vec(&serde_json::json!({"version":2,"guest_cwd":"/workspace",
            "bindings":[{"id":"sdk","workspace_path":GUEST_CODEX_HOME,"manifest_id":digest,
            "manifest_file":"sdk.manifest.json","access":"read-write"}]}))
        .map_err(|error| DataError::Other(error.to_string()))?;
        write_exclusive(directory.path(), "request.json", &request)?;
        let name = format!("awman-altana-auth-{}", uuid::Uuid::new_v4().simple());
        let token = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let intent = serde_json::to_vec(
            &serde_json::json!({"version":1,"container_name":name,"launch_token":token}),
        )
        .map_err(|error| DataError::Other(error.to_string()))?;
        write_exclusive(directory.path(), "launch-intent.json", &intent)?;
        let gate = load_startup_gate(directory.path(), Duration::from_secs(120))
            .map_err(|error| DataError::Other(error.to_string()))?;
        let controls = gate
            .control
            .orchestrated_parts()
            .ok_or_else(|| DataError::Other("auth controls must be protected".into()))?
            .0;
        let release = Some(AuthReleaseDestination {
            controls: Arc::clone(controls),
        });
        Ok(Self {
            _directory: directory,
            gate,
            release,
        })
    }
}
fn write_exclusive(parent: &Path, name: &str, bytes: &[u8]) -> Result<(), DataError> {
    let path = parent.join(name);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let mut file = options
        .open(&path)
        .map_err(|error| DataError::io(&path, error))?;
    file.write_all(bytes)
        .map_err(|error| DataError::io(&path, error))?;
    file.sync_all().map_err(|error| DataError::io(&path, error))
}
