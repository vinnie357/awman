//! Unix held-handle implementation for the startup-gate data contract.

use super::{
    ContainerName, GatedLaunchIdentity, StartupGateControlLayout, StartupGateError,
    StartupGateRequest, StartupGateSpec, ValidatedGateRequestSnapshot,
};
#[cfg(test)]
#[allow(unused_imports)]
// Frozen private-child tests share the portable ancestor's value types.
use super::{StartupGateAccess, StartupGateBinding, StartupGateControlLayoutInner};
use serde::de::{Error as _, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use zeroize::Zeroizing;

const MAX_REQUEST: u64 = 4096;
const MAX_MANIFEST: u64 = 8 * 1024 * 1024;
const MAX_INTENT: usize = 512;
const GUEST_CONTROL: &str = "guest-control";
const INTENT_FILE: &str = "launch-intent.json";
const PLAN_FILE: &str = "launch-plan.json";

const RESERVED_GATE_NAMES: &[&str] = &[
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
];

#[derive(Clone, Copy, Eq, PartialEq, Hash)]
pub(crate) struct NativeFileIdentity {
    pub device: u64,
    pub inode: u64,
    pub owner_uid: u32,
    pub owner_gid: u32,
    pub mode: u32,
}

impl fmt::Debug for NativeFileIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NativeFileIdentity")
            .field("device", &self.device)
            .field("inode", &self.inode)
            .field("owner_uid", &self.owner_uid)
            .field("owner_gid", &self.owner_gid)
            .field("mode", &format_args!("{:o}", self.mode & 0o7777))
            .finish()
    }
}

struct PinnedDirInner {
    dir: File,
    identity: NativeFileIdentity,
}

#[derive(Clone)]
pub(crate) struct PinnedControlDir(Arc<PinnedDirInner>);

impl PartialEq for PinnedControlDir {
    fn eq(&self, other: &Self) -> bool {
        self.0.identity == other.0.identity
    }
}

impl Eq for PinnedControlDir {}

impl fmt::Debug for PinnedControlDir {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("PinnedControlDir")
            .field(&self.0.identity)
            .finish()
    }
}

struct PinnedRegularFileInner {
    _file: File,
    identity: NativeFileIdentity,
}

#[derive(Clone)]
pub(crate) struct PinnedRegularFile(Arc<PinnedRegularFileInner>);

impl PartialEq for PinnedRegularFile {
    fn eq(&self, other: &Self) -> bool {
        self.0.identity == other.0.identity
    }
}

impl Eq for PinnedRegularFile {}

impl fmt::Debug for PinnedRegularFile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("PinnedRegularFile")
            .field(&self.0.identity)
            .finish()
    }
}

#[derive(Clone)]
pub(crate) struct PinnedGuestControl {
    dir: Arc<PinnedDirInner>,
    parent: PinnedControlDir,
    basename: &'static str,
}

impl PartialEq for PinnedGuestControl {
    fn eq(&self, other: &Self) -> bool {
        self.dir.identity == other.dir.identity
            && self.parent == other.parent
            && self.basename == other.basename
    }
}

impl Eq for PinnedGuestControl {}

impl fmt::Debug for PinnedGuestControl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PinnedGuestControl")
            .field("identity", &self.dir.identity)
            .field("parent_identity", &self.parent.0.identity)
            .field("basename", &self.basename)
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct CapturedControlLocator {
    parent_absolute: Arc<PathBuf>,
    child_basename: &'static str,
}

impl fmt::Debug for CapturedControlLocator {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CapturedControlLocator")
            .field("child_basename", &self.child_basename)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct OrchestratedControlAuthority {
    pub host_parent: PinnedControlDir,
    pub guest_control: PinnedGuestControl,
    pub locator: CapturedControlLocator,
    pub request: ValidatedGateRequestSnapshot,
    validated_manifests: Arc<BTreeMap<String, Vec<u8>>>,
}

impl OrchestratedControlAuthority {
    pub(crate) fn validated_manifests(&self) -> &BTreeMap<String, Vec<u8>> {
        self.validated_manifests.as_ref()
    }
}

impl fmt::Debug for OrchestratedControlAuthority {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OrchestratedControlAuthority")
            .field("host_parent", &self.host_parent)
            .field("guest_control", &self.guest_control)
            .finish_non_exhaustive()
    }
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
        D: Deserializer<'de>,
    {
        struct DigestVisitor;

        impl<'de> Visitor<'de> for DigestVisitor {
            type Value = RequiredNullableDigest;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
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

struct BorrowedIntent<'a> {
    version: u64,
    container_name: &'a str,
    launch_token: &'a str,
}

impl<'de> Deserialize<'de> for BorrowedIntent<'de> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct IntentVisitor;

        impl<'de> Visitor<'de> for IntentVisitor {
            type Value = BorrowedIntent<'de>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("the launch intention object")
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut version = None;
                let mut container_name = None;
                let mut launch_token = None;
                while let Some(key) = map.next_key::<&'de str>()? {
                    match key {
                        "version" if version.is_none() => version = Some(map.next_value::<u64>()?),
                        "container_name" if container_name.is_none() => {
                            container_name = Some(map.next_value::<&'de str>()?)
                        }
                        "launch_token" if launch_token.is_none() => {
                            launch_token = Some(map.next_value::<&'de str>()?)
                        }
                        "version" | "container_name" | "launch_token" => {
                            return Err(M::Error::custom("duplicate launch intention field"));
                        }
                        _ => return Err(M::Error::custom("unknown launch intention field")),
                    }
                }
                Ok(BorrowedIntent {
                    version: version
                        .ok_or_else(|| M::Error::custom("missing launch intention field"))?,
                    container_name: container_name
                        .ok_or_else(|| M::Error::custom("missing launch intention field"))?,
                    launch_token: launch_token
                        .ok_or_else(|| M::Error::custom("missing launch intention field"))?,
                })
            }
        }

        deserializer.deserialize_map(IntentVisitor)
    }
}

fn invalid(message: &'static str) -> StartupGateError {
    StartupGateError::OrchestratedInvalid(message)
}

fn io(source: std::io::Error) -> StartupGateError {
    StartupGateError::OrchestratedIo(source)
}

fn legacy_invalid(message: impl Into<String>) -> StartupGateError {
    StartupGateError::Invalid(message.into())
}

fn legacy_io(path: &Path, source: std::io::Error) -> StartupGateError {
    StartupGateError::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn identity(metadata: &std::fs::Metadata) -> NativeFileIdentity {
    NativeFileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        owner_uid: metadata.uid(),
        owner_gid: metadata.gid(),
        mode: metadata.mode(),
    }
}

fn effective_uid() -> u32 {
    nix::unistd::geteuid().as_raw()
}

fn validate_directory_metadata(
    metadata: &std::fs::Metadata,
) -> Result<NativeFileIdentity, StartupGateError> {
    if !metadata.file_type().is_dir()
        || metadata.mode() & 0o7777 != 0o700
        || metadata.uid() != effective_uid()
    {
        return Err(invalid("unsafe control directory"));
    }
    Ok(identity(metadata))
}

fn validate_regular_metadata(
    metadata: &std::fs::Metadata,
    mode: u32,
    max: u64,
    owner: u32,
) -> Result<NativeFileIdentity, StartupGateError> {
    if !metadata.file_type().is_file()
        || metadata.mode() & 0o7777 != mode
        || metadata.nlink() != 1
        || metadata.uid() != owner
        || metadata.len() > max
    {
        return Err(invalid("unsafe startup gate file"));
    }
    Ok(identity(metadata))
}

fn open_directory(path: &Path) -> Result<File, StartupGateError> {
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
        .map_err(io)
}

fn openat_readonly(parent: &File, name: &str, directory: bool) -> std::io::Result<File> {
    let mut flags = nix::fcntl::OFlag::O_RDONLY
        | nix::fcntl::OFlag::O_NOFOLLOW
        | nix::fcntl::OFlag::O_CLOEXEC
        | nix::fcntl::OFlag::O_NONBLOCK;
    if directory {
        flags |= nix::fcntl::OFlag::O_DIRECTORY;
    }
    nix::fcntl::openat(parent, name, flags, nix::sys::stat::Mode::empty())
        .map(File::from)
        .map_err(std::io::Error::from)
}

fn relative_entry_exists(parent: &File, name: &str) -> Result<bool, StartupGateError> {
    match nix::sys::stat::fstatat(parent, name, nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW) {
        Ok(_) => Ok(true),
        Err(nix::errno::Errno::ENOENT) => Ok(false),
        Err(error) => Err(io(error.into())),
    }
}

fn open_parent(control_dir: &Path) -> Result<(PinnedControlDir, PathBuf), StartupGateError> {
    let before = std::fs::symlink_metadata(control_dir).map_err(io)?;
    let before_identity = validate_directory_metadata(&before)?;
    let dir = open_directory(control_dir)?;
    let held_identity = validate_directory_metadata(&dir.metadata().map_err(io)?)?;
    if before_identity != held_identity {
        return Err(invalid("control directory changed during validation"));
    }
    let absolute = std::fs::canonicalize(control_dir).map_err(io)?;
    if !absolute.is_absolute() {
        return Err(invalid("control directory is not absolute"));
    }
    Ok((
        PinnedControlDir(Arc::new(PinnedDirInner {
            dir,
            identity: held_identity,
        })),
        absolute,
    ))
}

fn open_child(parent: &PinnedControlDir) -> Result<PinnedGuestControl, StartupGateError> {
    let dir = openat_readonly(&parent.0.dir, GUEST_CONTROL, true).map_err(io)?;
    let child_identity = validate_directory_metadata(&dir.metadata().map_err(io)?)?;
    if child_identity.device == parent.0.identity.device
        && child_identity.inode == parent.0.identity.inode
    {
        return Err(invalid("guest control aliases its parent"));
    }
    Ok(PinnedGuestControl {
        dir: Arc::new(PinnedDirInner {
            dir,
            identity: child_identity,
        }),
        parent: parent.clone(),
        basename: GUEST_CONTROL,
    })
}

fn open_regular_relative(
    parent: &PinnedControlDir,
    name: &'static str,
    mode: u32,
    max: u64,
) -> Result<File, StartupGateError> {
    let file = openat_readonly(&parent.0.dir, name, false).map_err(io)?;
    validate_regular_metadata(
        &file.metadata().map_err(io)?,
        mode,
        max,
        parent.0.identity.owner_uid,
    )?;
    Ok(file)
}

fn open_manifest_relative(parent: &PinnedControlDir, name: &str) -> Result<File, StartupGateError> {
    let file = openat_readonly(&parent.0.dir, name, false).map_err(io)?;
    validate_regular_metadata(
        &file.metadata().map_err(io)?,
        0o600,
        MAX_MANIFEST,
        parent.0.identity.owner_uid,
    )?;
    Ok(file)
}

fn read_bounded(mut file: File, max: u64) -> Result<Vec<u8>, StartupGateError> {
    let mut raw = Vec::new();
    Read::by_ref(&mut file)
        .take(max + 1)
        .read_to_end(&mut raw)
        .map_err(io)?;
    if raw.len() as u64 > max {
        return Err(invalid("oversized startup gate file"));
    }
    Ok(raw)
}

fn intent_exists(parent: &PinnedControlDir) -> Result<bool, StartupGateError> {
    match openat_readonly(&parent.0.dir, INTENT_FILE, false) {
        Ok(file) => {
            validate_regular_metadata(
                &file.metadata().map_err(io)?,
                0o600,
                MAX_INTENT as u64,
                parent.0.identity.owner_uid,
            )?;
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io(error)),
    }
}

fn parse_launch_intent(
    parent: &PinnedControlDir,
    request_digest: [u8; 32],
) -> Result<GatedLaunchIdentity, StartupGateError> {
    let mut file = open_regular_relative(parent, INTENT_FILE, 0o600, MAX_INTENT as u64)?;
    let mut raw = Zeroizing::new(Vec::with_capacity(MAX_INTENT + 1));
    raw.resize(MAX_INTENT + 1, 0);
    let mut used = 0usize;
    while used < raw.len() {
        let count = file.read(&mut raw[used..]).map_err(io)?;
        if count == 0 {
            break;
        }
        used += count;
    }
    if used > MAX_INTENT {
        return Err(invalid("launch intention exceeds 512 bytes"));
    }
    if raw[..used].iter().any(|byte| {
        !byte.is_ascii()
            || *byte == b'\\'
            || (byte.is_ascii_control() && !matches!(*byte, b'\t' | b'\n' | b'\r'))
    }) {
        return Err(invalid("launch intention has invalid lexical content"));
    }
    let identity = {
        let mut deserializer = serde_json::Deserializer::from_slice(&raw[..used]);
        let intent = BorrowedIntent::deserialize(&mut deserializer).map_err(|error| {
            StartupGateError::InvalidIntent {
                line: error.line(),
                column: error.column(),
            }
        })?;
        deserializer
            .end()
            .map_err(|error| StartupGateError::InvalidIntent {
                line: error.line(),
                column: error.column(),
            })?;
        if intent.version != 1 {
            return Err(invalid("unsupported launch intention version"));
        }
        if !valid_container_name(intent.container_name) {
            return Err(invalid("invalid orchestrated container name"));
        }
        if !hex64(intent.launch_token) {
            return Err(invalid("invalid launch token"));
        }
        let mut decoded = Zeroizing::new([0u8; 32]);
        for (index, pair) in intent.launch_token.as_bytes().chunks_exact(2).enumerate() {
            decoded[index] = (hex_nibble(pair[0]) << 4) | hex_nibble(pair[1]);
        }
        let mut hasher = Sha256::new();
        hasher.update(b"awman-launch-token-v1\0");
        hasher.update(decoded.as_slice());
        GatedLaunchIdentity {
            container_name: ContainerName::new(intent.container_name),
            token_digest: hasher.finalize().into(),
            request_digest,
        }
    };
    drop(raw);
    Ok(identity)
}

fn hex_nibble(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        _ => 0,
    }
}

fn hex64(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_container_name(value: &str) -> bool {
    value.len() <= 63
        && value.strip_prefix("awman-altana-").is_some_and(|suffix| {
            !suffix.is_empty()
                && !suffix.ends_with('-')
                && suffix
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        })
}

fn safe_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
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
            .all(|component| matches!(component, Component::RootDir | Component::Normal(_)))
}

fn overlaps(first: &Path, second: &Path) -> bool {
    first == second || first.starts_with(second) || second.starts_with(first)
}

fn is_reserved_gate_name(name: &str) -> bool {
    RESERVED_GATE_NAMES.contains(&name)
}

fn safe_manifest_name(value: &str) -> bool {
    !value.is_empty()
        && !is_reserved_gate_name(value)
        && Path::new(value).components().count() == 1
        && matches!(
            Path::new(value).components().next(),
            Some(Component::Normal(_))
        )
}

fn safe_legacy_manifest_name(value: &str) -> bool {
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

fn validate_manifest(raw: &[u8]) -> Result<(), StartupGateError> {
    if raw.starts_with(&[0xef, 0xbb, 0xbf]) {
        return Err(invalid("manifest has a UTF-8 BOM"));
    }
    let manifest: Manifest =
        serde_json::from_slice(raw).map_err(|_| invalid("invalid manifest JSON"))?;
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
                .any(|part| part.is_empty() || part == "." || part == "..")
        {
            return Err(invalid("invalid manifest entry path"));
        }
        if previous.is_some_and(|candidate| candidate >= bytes) {
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

fn validate_legacy_file(
    path: &Path,
    mode: u32,
    max: u64,
    owner: u32,
) -> Result<Vec<u8>, StartupGateError> {
    let before = std::fs::symlink_metadata(path).map_err(|error| legacy_io(path, error))?;
    if !before.file_type().is_file()
        || before.mode() & 0o777 != mode
        || before.nlink() != 1
        || before.uid() != owner
        || before.len() > max
    {
        return Err(legacy_invalid(format!("unsafe file {}", path.display())));
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| legacy_io(path, error))?;
    let metadata = file.metadata().map_err(|error| legacy_io(path, error))?;
    if !metadata.file_type().is_file()
        || metadata.mode() & 0o777 != mode
        || metadata.nlink() != 1
        || metadata.uid() != owner
        || metadata.len() > max
    {
        return Err(legacy_invalid(format!("unsafe file {}", path.display())));
    }
    if before.dev() != metadata.dev() || before.ino() != metadata.ino() {
        return Err(legacy_invalid(format!(
            "file changed during validation: {}",
            path.display()
        )));
    }
    let mut raw = Vec::new();
    file.take(max + 1)
        .read_to_end(&mut raw)
        .map_err(|error| legacy_io(path, error))?;
    if raw.len() as u64 > max {
        return Err(legacy_invalid(format!("oversized file {}", path.display())));
    }
    Ok(raw)
}

fn validate_legacy_manifest(raw: &[u8]) -> Result<(), StartupGateError> {
    if raw.starts_with(&[0xef, 0xbb, 0xbf]) {
        return Err(legacy_invalid("manifest has a UTF-8 BOM"));
    }
    let manifest: Manifest = serde_json::from_slice(raw)
        .map_err(|error| legacy_invalid(format!("invalid manifest JSON: {error}")))?;
    if manifest.version != 1 {
        return Err(legacy_invalid("unsupported manifest version"));
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
                .any(|part| part.is_empty() || part == "." || part == "..")
        {
            return Err(legacy_invalid("invalid manifest entry path"));
        }
        if previous.is_some_and(|candidate| candidate >= bytes) {
            return Err(legacy_invalid("manifest entries are not strictly sorted"));
        }
        previous = Some(bytes);
        match entry.kind.as_str() {
            "directory" if entry.size == 0 && entry.sha256.0.is_none() => {}
            "file" if entry.sha256.0.as_deref().is_some_and(hex64) => {}
            _ => return Err(legacy_invalid("invalid manifest entry")),
        }
    }
    Ok(())
}

fn load_legacy_startup_gate(
    control_dir: &Path,
    timeout: Duration,
) -> Result<StartupGateSpec, StartupGateError> {
    let metadata =
        std::fs::symlink_metadata(control_dir).map_err(|error| legacy_io(control_dir, error))?;
    if !metadata.file_type().is_dir()
        || metadata.mode() & 0o777 != 0o700
        || metadata.uid() != nix::unistd::Uid::current().as_raw()
    {
        return Err(legacy_invalid(
            "control directory must be a current-user-owned non-symlink mode-0700 directory",
        ));
    }
    for stale in ["ready.json", "release.json", "failure.json", ".released"] {
        let path = control_dir.join(stale);
        match std::fs::symlink_metadata(&path) {
            Ok(_) => return Err(legacy_invalid(format!("stale {stale}"))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(legacy_io(&path, error)),
        }
    }
    let request_path = control_dir.join("request.json");
    let raw = validate_legacy_file(&request_path, 0o600, MAX_REQUEST, metadata.uid())?;
    let request_digest = Sha256::digest(&raw).into();
    let request: StartupGateRequest = serde_json::from_slice(&raw)
        .map_err(|error| legacy_invalid(format!("invalid request JSON: {error}")))?;
    if request.version != 1 || request.bindings.is_empty() {
        return Err(legacy_invalid(
            "unsupported request version or empty bindings",
        ));
    }
    let mut ids = HashSet::new();
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut manifests = BTreeMap::new();
    for binding in &request.bindings {
        if !safe_id(&binding.id) || !ids.insert(binding.id.clone()) {
            return Err(legacy_invalid("invalid or duplicate binding id"));
        }
        if !normalized_absolute(&binding.workspace_path) {
            return Err(legacy_invalid("binding path is not normalized absolute"));
        }
        let path = PathBuf::from(&binding.workspace_path);
        let allowed = ["/workspace", "/review", "/work", "/data", "/mnt", "/output"];
        if !allowed
            .iter()
            .any(|root| path == Path::new(*root) || path.starts_with(*root))
        {
            return Err(legacy_invalid(
                "binding path is outside the allowed source roots",
            ));
        }
        for forbidden in ["/proc", "/sys", "/dev", "/run", "/etc", "/home", "/usr"] {
            if overlaps(&path, Path::new(forbidden)) {
                return Err(legacy_invalid("binding path overlaps a forbidden root"));
            }
        }
        if paths.iter().any(|prior| overlaps(&path, prior)) {
            return Err(legacy_invalid("binding paths overlap"));
        }
        paths.push(path);
        if !hex64(&binding.manifest_id) || !safe_legacy_manifest_name(&binding.manifest_file) {
            return Err(legacy_invalid("invalid manifest reference"));
        }
        let manifest_path = control_dir.join(&binding.manifest_file);
        let manifest_raw =
            validate_legacy_file(&manifest_path, 0o600, MAX_MANIFEST, metadata.uid())?;
        let actual: String = Sha256::digest(&manifest_raw)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        if actual != binding.manifest_id {
            return Err(legacy_invalid("manifest digest mismatch"));
        }
        validate_legacy_manifest(&manifest_raw)?;
        manifests.insert(binding.manifest_file.clone(), manifest_raw);
    }
    let canonical =
        std::fs::canonicalize(control_dir).map_err(|error| legacy_io(control_dir, error))?;
    Ok(StartupGateSpec {
        control: StartupGateControlLayout::legacy(canonical),
        request,
        request_digest,
        timeout,
        validated_manifests: manifests,
    })
}

type ValidatedRequestParts = (
    StartupGateRequest,
    Vec<u8>,
    [u8; 32],
    BTreeMap<String, Vec<u8>>,
);

fn validate_request(parent: &PinnedControlDir) -> Result<ValidatedRequestParts, StartupGateError> {
    let raw = read_bounded(
        open_regular_relative(parent, "request.json", 0o600, MAX_REQUEST)?,
        MAX_REQUEST,
    )?;
    let request_digest = Sha256::digest(&raw).into();
    let request: StartupGateRequest =
        serde_json::from_slice(&raw).map_err(|_| invalid("invalid request JSON"))?;
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
        for forbidden in ["/proc", "/sys", "/dev", "/run", "/etc", "/home", "/usr"] {
            if overlaps(&path, Path::new(forbidden)) {
                return Err(invalid("binding path overlaps a forbidden root"));
            }
        }
        if paths.iter().any(|prior| overlaps(&path, prior)) {
            return Err(invalid("binding paths overlap"));
        }
        paths.push(path);
        if !hex64(&binding.manifest_id) || !safe_manifest_name(&binding.manifest_file) {
            return Err(invalid("invalid manifest reference"));
        }
        let manifest_raw = read_bounded(
            open_manifest_relative(parent, &binding.manifest_file)?,
            MAX_MANIFEST,
        )?;
        let actual_hex: String = Sha256::digest(&manifest_raw)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        if actual_hex != binding.manifest_id {
            return Err(invalid("manifest digest mismatch"));
        }
        validate_manifest(&manifest_raw)?;
        manifests.insert(binding.manifest_file.clone(), manifest_raw);
    }
    Ok((request, raw, request_digest, manifests))
}

pub(crate) fn revalidate_mount_source(
    authority: &OrchestratedControlAuthority,
) -> Result<PathBuf, StartupGateError> {
    let held_parent =
        validate_directory_metadata(&authority.host_parent.0.dir.metadata().map_err(io)?)?;
    let held_child =
        validate_directory_metadata(&authority.guest_control.dir.dir.metadata().map_err(io)?)?;
    if held_parent != authority.host_parent.0.identity
        || held_child != authority.guest_control.dir.identity
    {
        return Err(invalid("held control identity changed"));
    }
    let current_parent = open_directory(authority.locator.parent_absolute.as_path())?;
    if validate_directory_metadata(&current_parent.metadata().map_err(io)?)? != held_parent {
        return Err(invalid("control parent identity changed"));
    }
    let current_child = openat_readonly(
        &authority.host_parent.0.dir,
        authority.locator.child_basename,
        true,
    )
    .map_err(io)?;
    if validate_directory_metadata(&current_child.metadata().map_err(io)?)? != held_child {
        return Err(invalid("guest control identity changed"));
    }
    Ok(authority
        .locator
        .parent_absolute
        .join(authority.locator.child_basename))
}

fn validate_guest_control_initial(
    guest: &PinnedGuestControl,
    manifests: &BTreeMap<String, Vec<u8>>,
) -> Result<(), StartupGateError> {
    for name in RESERVED_GATE_NAMES {
        if relative_entry_exists(&guest.dir.dir, name)? {
            return Err(invalid("guest control contains a reserved entry"));
        }
    }
    for name in manifests.keys() {
        if relative_entry_exists(&guest.dir.dir, name)? {
            return Err(invalid("guest control contains a manifest entry"));
        }
    }
    Ok(())
}

pub fn load_startup_gate(
    control_dir: &Path,
    timeout: Duration,
) -> Result<StartupGateSpec, StartupGateError> {
    if !(1..=3600).contains(&timeout.as_secs()) || timeout.subsec_nanos() != 0 {
        return Err(legacy_invalid("timeout must be whole seconds in 1..=3600"));
    }
    match std::fs::symlink_metadata(control_dir.join(INTENT_FILE)) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return load_legacy_startup_gate(control_dir, timeout);
        }
        Err(error) => return Err(io(error)),
    }
    let (parent, absolute) = open_parent(control_dir)?;
    if !intent_exists(&parent)? {
        return Err(invalid("launch intention changed during validation"));
    }
    let (request, request_bytes, request_digest, validated_manifests) = validate_request(&parent)?;
    let guest_control = open_child(&parent)?;
    validate_guest_control_initial(&guest_control, &validated_manifests)?;
    let identity = parse_launch_intent(&parent, request_digest)?;
    let request_snapshot = ValidatedGateRequestSnapshot {
        request: Arc::new(request.clone()),
        request_digest,
        request_bytes: Arc::new(request_bytes),
    };
    let controls = Arc::new(OrchestratedControlAuthority {
        host_parent: parent,
        guest_control,
        locator: CapturedControlLocator {
            parent_absolute: Arc::new(absolute),
            child_basename: GUEST_CONTROL,
        },
        request: request_snapshot,
        validated_manifests: Arc::new(validated_manifests.clone()),
    });
    revalidate_mount_source(&controls)?;
    let control = StartupGateControlLayout::orchestrated(controls, identity);
    Ok(StartupGateSpec {
        control,
        request,
        request_digest,
        timeout,
        validated_manifests,
    })
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum PlanFileError {
    #[error("launch plan already exists or is unsafe")]
    ExistingOrUnsafe,
    #[error("launch plan publication failed")]
    PublicationFailed,
}

impl PinnedControlDir {
    pub(crate) fn existing_launch_plan(&self) -> Result<Option<PinnedRegularFile>, PlanFileError> {
        match openat_readonly(&self.0.dir, PLAN_FILE, false) {
            Ok(file) => {
                let metadata = file
                    .metadata()
                    .map_err(|_| PlanFileError::ExistingOrUnsafe)?;
                let identity = validate_regular_metadata(
                    &metadata,
                    0o600,
                    MAX_REQUEST,
                    self.0.identity.owner_uid,
                )
                .map_err(|_| PlanFileError::ExistingOrUnsafe)?;
                Ok(Some(PinnedRegularFile(Arc::new(PinnedRegularFileInner {
                    _file: file,
                    identity,
                }))))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err(PlanFileError::ExistingOrUnsafe),
        }
    }

    pub(crate) fn publish_launch_plan(
        &self,
        bytes: &[u8],
    ) -> Result<PinnedRegularFile, PlanFileError> {
        if self.existing_launch_plan()?.is_some() {
            return Err(PlanFileError::ExistingOrUnsafe);
        }
        let temp_name = format!(".launch-plan.{}.tmp", uuid::Uuid::new_v4().simple());
        let fd = nix::fcntl::openat(
            &self.0.dir,
            temp_name.as_str(),
            nix::fcntl::OFlag::O_WRONLY
                | nix::fcntl::OFlag::O_CREAT
                | nix::fcntl::OFlag::O_EXCL
                | nix::fcntl::OFlag::O_NOFOLLOW
                | nix::fcntl::OFlag::O_CLOEXEC,
            nix::sys::stat::Mode::from_bits_truncate(0o600),
        )
        .map_err(|_| PlanFileError::PublicationFailed)?;
        let mut file = File::from(fd);
        let result = (|| {
            validate_regular_metadata(
                &file
                    .metadata()
                    .map_err(|_| PlanFileError::PublicationFailed)?,
                0o600,
                MAX_REQUEST,
                self.0.identity.owner_uid,
            )
            .map_err(|_| PlanFileError::PublicationFailed)?;
            file.write_all(bytes)
                .map_err(|_| PlanFileError::PublicationFailed)?;
            file.sync_all()
                .map_err(|_| PlanFileError::PublicationFailed)?;
            rename_noreplace(&self.0.dir, temp_name.as_str(), PLAN_FILE)?;
            self.0
                .dir
                .sync_all()
                .map_err(|_| PlanFileError::PublicationFailed)?;
            let metadata = file
                .metadata()
                .map_err(|_| PlanFileError::PublicationFailed)?;
            let identity =
                validate_regular_metadata(&metadata, 0o600, MAX_REQUEST, self.0.identity.owner_uid)
                    .map_err(|_| PlanFileError::PublicationFailed)?;
            Ok(PinnedRegularFile(Arc::new(PinnedRegularFileInner {
                _file: file,
                identity,
            })))
        })();
        if result.is_err() {
            let _ = nix::unistd::unlinkat(
                &self.0.dir,
                temp_name.as_str(),
                nix::unistd::UnlinkatFlags::NoRemoveDir,
            );
        }
        result
    }

    pub(crate) fn verify_launch_plan(&self, expected: &PinnedRegularFile) -> bool {
        matches!(self.existing_launch_plan(), Ok(Some(actual)) if actual == *expected)
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn rename_noreplace(
    parent: &File,
    source: &str,
    target: &'static str,
) -> Result<(), PlanFileError> {
    match rustix::fs::renameat_with(
        parent,
        source,
        parent,
        target,
        rustix::fs::RenameFlags::NOREPLACE,
    ) {
        Ok(()) => Ok(()),
        Err(rustix::io::Errno::EXIST) => Err(PlanFileError::ExistingOrUnsafe),
        Err(_) => Err(PlanFileError::PublicationFailed),
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn rename_noreplace(
    _parent: &File,
    _source: &str,
    _target: &'static str,
) -> Result<(), PlanFileError> {
    Err(PlanFileError::PublicationFailed)
}

#[cfg(test)]
#[path = "startup_gate_native_p2_test.rs"]
mod native_p2;
