//! Fresh main controls bind current SDK content and the selected read-only workspace.
use super::native_codex_auth::GUEST_CODEX_HOME;
use crate::data::error::DataError;
use crate::data::startup_gate::{
    load_startup_gate, OrchestratedControlAuthority, PlanFileError, StartupGateSpec,
};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

pub(crate) struct CodexMainLaunchFiles {
    _directory: tempfile::TempDir,
    pub(crate) gate: StartupGateSpec,
    release: Option<MainReleaseDestination>,
}
pub(crate) struct MainReleaseDestination {
    controls: Arc<OrchestratedControlAuthority>,
}
impl MainReleaseDestination {
    pub(crate) fn matches_controls(&self, controls: &Arc<OrchestratedControlAuthority>) -> bool {
        Arc::ptr_eq(&self.controls, controls)
    }
    pub(crate) fn publish(self, nonce: &str) -> Result<(), PlanFileError> {
        #[cfg(unix)]
        {
            self.controls.publish_main_release(nonce)
        }
        #[cfg(not(unix))]
        {
            let _ = nonce;
            Err(PlanFileError::PublicationFailed)
        }
    }
}
impl CodexMainLaunchFiles {
    pub(crate) fn take_release_destination(&mut self) -> Option<MainReleaseDestination> {
        self.release.take()
    }
    pub(crate) fn new(root: &Path, sdk: &Path, project: &Path) -> Result<Self, DataError> {
        let mut builder = tempfile::Builder::new();
        builder.prefix("native-codex-main-");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            builder.permissions(std::fs::Permissions::from_mode(0o700));
        }
        let directory = builder
            .tempdir_in(root)
            .map_err(|e| DataError::io(root, e))?;
        let controls = std::fs::canonicalize(directory.path())
            .map_err(|e| DataError::io(directory.path(), e))?;
        for source in [sdk, project] {
            if source.starts_with(&controls) || controls.starts_with(source) {
                return Err(invalid("main source overlaps controls"));
            }
        }
        let guest = directory.path().join("guest-control");
        std::fs::create_dir(&guest).map_err(|e| DataError::io(&guest, e))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&guest, std::fs::Permissions::from_mode(0o700))
                .map_err(|e| DataError::io(&guest, e))?;
        }
        let mut bindings = Vec::new();
        for (id, source, target, access) in [
            ("sdk", sdk, GUEST_CODEX_HOME, "read-write"),
            ("workspace", project, "/workspace", "read-only"),
        ] {
            let manifest = snapshot(source)?;
            let name = format!("{id}.manifest.json");
            write_exclusive(directory.path(), &name, &manifest)?;
            bindings.push(serde_json::json!({"id":id,"workspace_path":target,"manifest_id":hex(&manifest),"manifest_file":name,"access":access}));
        }
        write_exclusive(
            directory.path(),
            "request.json",
            &serde_json::to_vec(
                &serde_json::json!({"version":2,"guest_cwd":"/workspace","bindings":bindings}),
            )
            .map_err(|e| invalid(&e.to_string()))?,
        )?;
        let intent = serde_json::json!({"version":1,"container_name":format!("awman-altana-main-{}",uuid::Uuid::new_v4().simple()),"launch_token":format!("{}{}",uuid::Uuid::new_v4().simple(),uuid::Uuid::new_v4().simple())});
        write_exclusive(
            directory.path(),
            "launch-intent.json",
            &serde_json::to_vec(&intent).map_err(|e| invalid(&e.to_string()))?,
        )?;
        let gate = load_startup_gate(directory.path(), Duration::from_secs(120))
            .map_err(|e| invalid(&e.to_string()))?;
        let authority = gate
            .control
            .orchestrated_parts()
            .ok_or_else(|| invalid("main controls require protected authority"))?
            .0;
        let release = Some(MainReleaseDestination {
            controls: Arc::clone(authority),
        });
        Ok(Self {
            _directory: directory,
            gate,
            release,
        })
    }
}
fn invalid(message: &str) -> DataError {
    DataError::Other(message.into())
}
fn hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
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
    let mut file = options.open(&path).map_err(|e| DataError::io(&path, e))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| DataError::io(&path, e))
}
#[cfg(not(unix))]
fn snapshot(_: &Path) -> Result<Vec<u8>, DataError> {
    Err(invalid("main manifests require Unix"))
}
#[cfg(unix)]
fn snapshot(root: &Path) -> Result<Vec<u8>, DataError> {
    use nix::fcntl::{openat, OFlag};
    use nix::sys::stat::Mode;
    use std::fs::{File, OpenOptions};
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    fn identity(m: &std::fs::Metadata) -> (u64, u64, u64, i64, i64, i64, i64) {
        (
            m.dev(),
            m.ino(),
            m.len(),
            m.mtime(),
            m.mtime_nsec(),
            m.ctime(),
            m.ctime_nsec(),
        )
    }
    fn child(dir: &File, name: &str, path: &Path) -> Result<File, DataError> {
        openat(
            dir,
            name,
            OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_NONBLOCK,
            Mode::empty(),
        )
        .map(File::from)
        .map_err(|e| DataError::io(path, e.into()))
    }
    fn walk(
        directory: File,
        root: &Path,
        relative: &Path,
        entries: &mut Vec<serde_json::Value>,
        total: &mut u64,
    ) -> Result<(), DataError> {
        let path = root.join(relative);
        let original = directory.metadata().map_err(|e| DataError::io(&path, e))?;
        let reader =
            rustix::fs::Dir::read_from(&directory).map_err(|e| DataError::io(&path, e.into()))?;
        let mut children = Vec::new();
        for entry in reader {
            let entry = entry.map_err(|e| DataError::io(&path, e.into()))?;
            let name = entry
                .file_name()
                .to_str()
                .map_err(|_| invalid("non-UTF8 main manifest name"))?;
            if name == "." || name == ".." {
                continue;
            }
            if children.len() >= 65536 {
                return Err(invalid("main manifest entry bound exceeded"));
            }
            children.push(name.to_owned());
        }
        children.sort();
        for name in children {
            if entries.len() >= 65536 || relative.components().count() >= 128 {
                return Err(invalid("main manifest entry/depth bound exceeded"));
            }
            if name.contains('\\') || name.contains('\0') {
                return Err(invalid("unsafe main manifest name"));
            }
            let child_relative = relative.join(&name);
            let child_path = root.join(&child_relative);
            let mut file = child(&directory, &name, &child_path)?;
            let held = file.metadata().map_err(|e| DataError::io(&child_path, e))?;
            let label = child_relative
                .to_str()
                .ok_or_else(|| invalid("invalid main manifest path"))?;
            if label.len() > 4096 {
                return Err(invalid("main manifest path bound exceeded"));
            }
            if held.is_dir() {
                entries.push(
                    serde_json::json!({"path":label,"kind":"directory","size":0,"sha256":null}),
                );
                walk(file, root, &child_relative, entries, total)?;
            } else if held.is_file() {
                *total = total
                    .checked_add(held.len())
                    .ok_or_else(|| invalid("main manifest size overflow"))?;
                if *total > 1024 * 1024 * 1024 {
                    return Err(invalid("main manifest byte bound exceeded"));
                }
                let mut hash = Sha256::new();
                let mut bytes = [0u8; 65536];
                let mut read = 0u64;
                loop {
                    let count = file
                        .read(&mut bytes)
                        .map_err(|e| DataError::io(&child_path, e))?;
                    if count == 0 {
                        break;
                    }
                    read += count as u64;
                    if read > held.len() {
                        return Err(invalid("main manifest file grew"));
                    }
                    hash.update(&bytes[..count]);
                }
                if read != held.len()
                    || identity(&file.metadata().map_err(|e| DataError::io(&child_path, e))?)
                        != identity(&held)
                {
                    return Err(invalid("main manifest file changed"));
                }
                let digest: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
                entries.push(
                    serde_json::json!({"path":label,"kind":"file","size":read,"sha256":digest}),
                );
            } else {
                return Err(invalid("unsupported main manifest entry"));
            }
            let named = child(&directory, &name, &child_path)?
                .metadata()
                .map_err(|e| DataError::io(&child_path, e))?;
            if identity(&held) != identity(&named) {
                return Err(invalid("main manifest entry changed"));
            }
        }
        let after = directory.metadata().map_err(|e| DataError::io(&path, e))?;
        if identity(&original) != identity(&after) {
            return Err(invalid("main manifest directory changed"));
        }
        Ok(())
    }
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(root)
        .map_err(|e| DataError::io(root, e))?;
    let mut entries = Vec::new();
    walk(directory, root, Path::new(""), &mut entries, &mut 0)?;
    entries.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
    let bytes = serde_json::to_vec(&serde_json::json!({"version":1,"entries":entries}))
        .map_err(|e| invalid(&e.to_string()))?;
    if bytes.len() > 8 * 1024 * 1024 {
        return Err(invalid("main manifest record bound exceeded"));
    }
    Ok(bytes)
}
