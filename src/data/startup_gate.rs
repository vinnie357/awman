//! Validated on-disk contract for the opt-in pre-agent startup gate.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

const MAX_REQUEST: u64 = 4096;
const MAX_MANIFEST: u64 = 8 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StartupGateRequest {
    pub version: u32,
    pub bindings: Vec<StartupGateBinding>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StartupGateBinding {
    pub id: String,
    pub workspace_path: String,
    pub manifest_id: String,
    pub manifest_file: String,
    pub access: StartupGateAccess,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum StartupGateAccess {
    ReadOnly,
    ReadWrite,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartupGateSpec {
    pub control_dir: PathBuf,
    pub request: StartupGateRequest,
    pub timeout: Duration,
    pub validated_manifests: BTreeMap<String, Vec<u8>>,
}

#[derive(Debug, thiserror::Error)]
pub enum StartupGateError {
    #[error("startup gates are unsupported on this platform")]
    UnsupportedPlatform,
    #[error("invalid startup gate: {0}")]
    Invalid(String),
    #[error("startup gate I/O at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    entries: Vec<ManifestEntry>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestEntry {
    path: String,
    kind: String,
    size: u64,
    sha256: RequiredNullableDigest,
}

struct RequiredNullableDigest(Option<String>);

impl<'de> Deserialize<'de> for RequiredNullableDigest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct DigestVisitor;

        impl<'de> serde::de::Visitor<'de> for DigestVisitor {
            type Value = RequiredNullableDigest;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a SHA-256 string or null")
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(RequiredNullableDigest(None))
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(RequiredNullableDigest(Some(value.to_owned())))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(RequiredNullableDigest(Some(value)))
            }
        }

        deserializer.deserialize_any(DigestVisitor)
    }
}

fn invalid(message: impl Into<String>) -> StartupGateError {
    StartupGateError::Invalid(message.into())
}
fn io(path: &Path, source: std::io::Error) -> StartupGateError {
    StartupGateError::Io {
        path: path.to_path_buf(),
        source,
    }
}
fn hex64(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn safe_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}
fn normalized_absolute(value: &str) -> bool {
    !value.contains('\0')
        && value.starts_with('/')
        && !value.ends_with('/')
        && !value
            .split('/')
            .skip(1)
            .any(|part| part.is_empty() || part == "." || part == "..")
        && Path::new(value)
            .components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
}
fn overlaps(a: &Path, b: &Path) -> bool {
    a == b || a.starts_with(b) || b.starts_with(a)
}
fn safe_manifest_name(value: &str) -> bool {
    !value.is_empty()
        && !matches!(
            value,
            "bootstrap.py"
                | "request.json"
                | "original-python-env.json"
                | ".released"
                | "ready.json"
                | "release.json"
                | "failure.json"
        )
        && Path::new(value).components().count() == 1
        && matches!(
            Path::new(value).components().next(),
            Some(Component::Normal(_))
        )
}

#[cfg(unix)]
fn validate_file(
    path: &Path,
    mode: u32,
    max: u64,
    owner: u32,
) -> Result<Vec<u8>, StartupGateError> {
    use std::io::Read as _;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let before = std::fs::symlink_metadata(path).map_err(|e| io(path, e))?;
    if !before.file_type().is_file()
        || before.mode() & 0o777 != mode
        || before.nlink() != 1
        || before.uid() != owner
        || before.len() > max
    {
        return Err(invalid(format!("unsafe file {}", path.display())));
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| io(path, e))?;
    let meta = file.metadata().map_err(|e| io(path, e))?;
    if !meta.file_type().is_file()
        || meta.mode() & 0o777 != mode
        || meta.nlink() != 1
        || meta.uid() != owner
        || meta.len() > max
    {
        return Err(invalid(format!("unsafe file {}", path.display())));
    }
    if before.dev() != meta.dev() || before.ino() != meta.ino() {
        return Err(invalid(format!(
            "file changed during validation: {}",
            path.display()
        )));
    }
    let mut raw = Vec::new();
    file.take(max + 1)
        .read_to_end(&mut raw)
        .map_err(|e| io(path, e))?;
    if raw.len() as u64 > max {
        return Err(invalid(format!("oversized file {}", path.display())));
    }
    Ok(raw)
}

fn validate_manifest(raw: &[u8]) -> Result<(), StartupGateError> {
    if raw.starts_with(&[0xef, 0xbb, 0xbf]) {
        return Err(invalid("manifest has a UTF-8 BOM"));
    }
    let manifest: Manifest =
        serde_json::from_slice(raw).map_err(|e| invalid(format!("invalid manifest JSON: {e}")))?;
    if manifest.version != 1 {
        return Err(invalid("unsupported manifest version"));
    }
    let mut previous: Option<&[u8]> = None;
    for entry in &manifest.entries {
        let bytes = entry.path.as_bytes();
        if bytes.is_empty()
            || entry.path.contains('\\')
            || entry.path.contains('\0')
            || entry.path.starts_with('/')
            || entry
                .path
                .split('/')
                .any(|p| p.is_empty() || p == "." || p == "..")
        {
            return Err(invalid("invalid manifest entry path"));
        }
        if previous.is_some_and(|p| p >= bytes) {
            return Err(invalid("manifest entries are not strictly sorted"));
        }
        previous = Some(bytes);
        match entry.kind.as_str() {
            "directory" if entry.size == 0 && entry.sha256.0.is_none() => {}
            "file" if entry.sha256.0.as_deref().is_some_and(hex64) => {}
            _ => return Err(invalid("invalid manifest entry")),
        }
    }
    Ok(())
}

#[cfg(unix)]
pub fn load_startup_gate(
    control_dir: &Path,
    timeout: Duration,
) -> Result<StartupGateSpec, StartupGateError> {
    use std::os::unix::fs::MetadataExt;
    if !(1..=3600).contains(&timeout.as_secs()) || timeout.subsec_nanos() != 0 {
        return Err(invalid("timeout must be whole seconds in 1..=3600"));
    }
    let meta = std::fs::symlink_metadata(control_dir).map_err(|e| io(control_dir, e))?;
    if !meta.file_type().is_dir()
        || meta.mode() & 0o777 != 0o700
        || meta.uid() != nix::unistd::Uid::current().as_raw()
    {
        return Err(invalid(
            "control directory must be a current-user-owned non-symlink mode-0700 directory",
        ));
    }
    for stale in ["ready.json", "release.json", "failure.json", ".released"] {
        match std::fs::symlink_metadata(control_dir.join(stale)) {
            Ok(_) => return Err(invalid(format!("stale {stale}"))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io(&control_dir.join(stale), error)),
        }
    }
    let request_path = control_dir.join("request.json");
    let raw = validate_file(&request_path, 0o600, MAX_REQUEST, meta.uid())?;
    let request: StartupGateRequest =
        serde_json::from_slice(&raw).map_err(|e| invalid(format!("invalid request JSON: {e}")))?;
    if request.version != 1 || request.bindings.is_empty() {
        return Err(invalid("unsupported request version or empty bindings"));
    }
    let mut ids = HashSet::new();
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut manifests = BTreeMap::new();
    for binding in &request.bindings {
        if !safe_id(&binding.id) || !ids.insert(binding.id.clone()) {
            return Err(invalid("invalid or duplicate binding id"));
        }
        if !normalized_absolute(&binding.workspace_path) {
            return Err(invalid("binding path is not normalized absolute"));
        }
        let path = PathBuf::from(&binding.workspace_path);
        let allowed = ["/workspace", "/review", "/work", "/data", "/mnt", "/output"];
        if !allowed
            .iter()
            .any(|root| path == Path::new(*root) || path.starts_with(*root))
        {
            return Err(invalid("binding path is outside the allowed source roots"));
        }
        if path == Path::new("/") {
            return Err(invalid("binding path cannot be root"));
        }
        for forbidden in ["/proc", "/sys", "/dev", "/run", "/etc", "/home", "/usr"] {
            if overlaps(&path, Path::new(forbidden)) {
                return Err(invalid("binding path overlaps a forbidden root"));
            }
        }
        if paths.iter().any(|p| overlaps(&path, p)) {
            return Err(invalid("binding paths overlap"));
        }
        paths.push(path);
        if !hex64(&binding.manifest_id) || !safe_manifest_name(&binding.manifest_file) {
            return Err(invalid("invalid manifest reference"));
        }
        let manifest_path = control_dir.join(&binding.manifest_file);
        let manifest_raw = validate_file(&manifest_path, 0o600, MAX_MANIFEST, meta.uid())?;
        let actual: String = Sha256::digest(&manifest_raw)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        if actual != binding.manifest_id {
            return Err(invalid("manifest digest mismatch"));
        }
        validate_manifest(&manifest_raw)?;
        manifests.insert(binding.manifest_file.clone(), manifest_raw);
    }
    let canonical = std::fs::canonicalize(control_dir).map_err(|e| io(control_dir, e))?;
    Ok(StartupGateSpec {
        control_dir: canonical,
        request,
        timeout,
        validated_manifests: manifests,
    })
}

#[cfg(not(unix))]
pub fn load_startup_gate(
    _control_dir: &Path,
    _timeout: Duration,
) -> Result<StartupGateSpec, StartupGateError> {
    Err(StartupGateError::UnsupportedPlatform)
}
