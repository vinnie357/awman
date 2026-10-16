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
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use zeroize::Zeroizing;

const MAX_REQUEST: u64 = 4096;
const MAX_MANIFEST: u64 = 8 * 1024 * 1024;
const MAX_INTENT: usize = 512;
const GUEST_CONTROL: &str = "guest-control";
const INTENT_FILE: &str = "launch-intent.json";
const PLAN_FILE: &str = "launch-plan.json";
const RECEIPT_FILE: &str = "launch.json";
const CLEANUP_FILE: &str = "cleanup.json";

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
    "cleanup.json",
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
    #[cfg(test)]
    fail_launch_plan_sync: Arc<AtomicBool>,
    #[cfg(test)]
    after_launch_plan_publication: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
    #[cfg(all(test, unix))]
    after_auth_release_rename: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
    #[cfg(all(test, unix))]
    auth_release_sync_fault: std::sync::Mutex<Option<Arc<AuthReleaseSyncFaultState>>>,
}

#[derive(Clone)]
pub(crate) struct PinnedControlDir(Arc<PinnedDirInner>);

#[cfg(test)]
pub(crate) struct LaunchPlanSyncFaultHandle {
    armed: Arc<AtomicBool>,
}

#[cfg(test)]
impl LaunchPlanSyncFaultHandle {
    pub(crate) fn clear(&self) {
        self.armed.store(false, Ordering::Release);
    }
}

#[cfg(test)]
impl Drop for LaunchPlanSyncFaultHandle {
    fn drop(&mut self) {
        self.clear();
    }
}

#[cfg(test)]
pub(crate) fn arm_launch_plan_post_rename_sync_failure(
    controls: &Arc<OrchestratedControlAuthority>,
) -> LaunchPlanSyncFaultHandle {
    let armed = Arc::clone(&controls.host_parent.0.fail_launch_plan_sync);
    assert!(
        !armed.swap(true, Ordering::AcqRel),
        "launch-plan sync fault is already armed for this control authority"
    );
    LaunchPlanSyncFaultHandle { armed }
}

#[cfg(test)]
pub(crate) fn after_launch_plan_native_publication(
    controls: &Arc<OrchestratedControlAuthority>,
    action: Box<dyn FnOnce() + Send>,
) {
    let mut slot = controls
        .host_parent
        .0
        .after_launch_plan_publication
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert!(
        slot.replace(action).is_none(),
        "launch-plan publication action is already installed"
    );
}

#[cfg(all(test, unix))]
pub(crate) fn after_auth_release_noreplace_rename(
    controls: &Arc<OrchestratedControlAuthority>,
    action: Box<dyn FnOnce() + Send>,
) {
    let mut slot = controls
        .guest_control
        .dir
        .after_auth_release_rename
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert!(
        slot.replace(action).is_none(),
        "auth release rename action is already installed"
    );
}

#[cfg(all(test, unix))]
struct AuthReleaseSyncFaultState {
    armed: AtomicBool,
    fired: AtomicBool,
}

#[cfg(all(test, unix))]
pub(crate) struct AuthReleaseSyncFaultHandle {
    state: Arc<AuthReleaseSyncFaultState>,
}

#[cfg(all(test, unix))]
impl AuthReleaseSyncFaultHandle {
    pub(crate) fn did_fire(&self) -> bool {
        self.state.fired.load(Ordering::Acquire)
    }
}

#[cfg(all(test, unix))]
impl Drop for AuthReleaseSyncFaultHandle {
    fn drop(&mut self) {
        self.state.armed.store(false, Ordering::Release);
    }
}

#[cfg(all(test, unix))]
pub(crate) fn arm_auth_release_post_rename_sync_failure(
    controls: &Arc<OrchestratedControlAuthority>,
) -> AuthReleaseSyncFaultHandle {
    let mut slot = controls
        .guest_control
        .dir
        .auth_release_sync_fault
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert!(
        slot.as_ref()
            .is_none_or(|state| !state.armed.load(Ordering::Acquire)),
        "auth release sync fault is already armed for this control authority"
    );
    let state = Arc::new(AuthReleaseSyncFaultState {
        armed: AtomicBool::new(true),
        fired: AtomicBool::new(false),
    });
    *slot = Some(Arc::clone(&state));
    AuthReleaseSyncFaultHandle { state }
}

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
    file: File,
    identity: NativeFileIdentity,
    bytes: Arc<Vec<u8>>,
}

#[derive(Clone)]
pub(crate) struct PinnedRegularFile(Arc<PinnedRegularFileInner>);

impl PartialEq for PinnedRegularFile {
    fn eq(&self, other: &Self) -> bool {
        self.0.identity == other.0.identity && self.0.bytes == other.0.bytes
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

impl PinnedRegularFile {
    #[cfg(test)]
    pub(crate) fn held_file_identity_observation(&self) -> Option<(u64, u64, u64)> {
        self.0
            .file
            .metadata()
            .ok()
            .map(|metadata| (metadata.dev(), metadata.ino(), metadata.nlink()))
    }

    pub(crate) fn exact_bytes(&self) -> &[u8] {
        self.0.bytes.as_slice()
    }

    pub(crate) fn held_bytes_match(&self) -> bool {
        let Ok(before_metadata) = self.0.file.metadata() else {
            return false;
        };
        if usize::try_from(before_metadata.len()).ok() != Some(self.0.bytes.len()) {
            return false;
        }
        let Ok(before) = validate_regular_metadata(
            &before_metadata,
            0o600,
            MAX_REQUEST,
            self.0.identity.owner_uid,
        ) else {
            return false;
        };
        if before != self.0.identity {
            return false;
        }
        let Ok(bytes) = read_bounded_at(&self.0.file, MAX_REQUEST) else {
            return false;
        };
        let Ok(after_metadata) = self.0.file.metadata() else {
            return false;
        };
        if usize::try_from(after_metadata.len()).ok() != Some(self.0.bytes.len()) {
            return false;
        }
        matches!(
            validate_regular_metadata(
                &after_metadata,
                0o600,
                MAX_REQUEST,
                self.0.identity.owner_uid,
            ),
            Ok(identity) if identity == self.0.identity && bytes == *self.0.bytes
        )
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
    // Fixed Layer0 persistence, not permission to release a launch. The sole
    // production caller is the opaque auth destination, behind engine custody.
    pub(in crate::data) fn publish_auth_release(&self, nonce: &str) -> Result<(), PlanFileError> {
        if !hex64(nonce) {
            return Err(PlanFileError::ExistingOrUnsafe);
        }
        revalidate_mount_source(self).map_err(|_| PlanFileError::ExistingOrUnsafe)?;
        let bytes = serde_json::to_vec(&serde_json::json!({"version":1,"nonce":nonce}))
            .map_err(|_| PlanFileError::PublicationFailed)?;
        let dir = &self.guest_control.dir.dir;
        let temp_name = format!(".auth-release.{}.tmp", uuid::Uuid::new_v4().simple());
        let fd = nix::fcntl::openat(
            dir,
            temp_name.as_str(),
            nix::fcntl::OFlag::O_RDWR
                | nix::fcntl::OFlag::O_CREAT
                | nix::fcntl::OFlag::O_EXCL
                | nix::fcntl::OFlag::O_NOFOLLOW
                | nix::fcntl::OFlag::O_CLOEXEC,
            nix::sys::stat::Mode::from_bits_truncate(0o600),
        )
        .map_err(|_| PlanFileError::PublicationFailed)?;
        let mut file = File::from(fd);
        let original = validate_regular_metadata(
            &file
                .metadata()
                .map_err(|_| PlanFileError::PublicationFailed)?,
            0o600,
            MAX_REQUEST,
            self.guest_control.dir.identity.owner_uid,
        )
        .map_err(|_| PlanFileError::ExistingOrUnsafe)?;
        file.write_all(&bytes)
            .map_err(|_| PlanFileError::PublicationFailed)?;
        file.sync_all()
            .map_err(|_| PlanFileError::PublicationFailed)?;
        revalidate_mount_source(self).map_err(|_| PlanFileError::ExistingOrUnsafe)?;
        // The private temporary name must still designate our exact held file.
        verify_auth_release_name(dir, &temp_name, &file, original, &bytes, false)?;
        rename_noreplace(dir, &temp_name, "release.json")?;
        #[cfg(all(test, unix))]
        {
            let action = {
                self.guest_control
                    .dir
                    .after_auth_release_rename
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take()
            };
            if let Some(action) = action {
                action();
            }
        }
        #[cfg(all(test, unix))]
        {
            let fault = {
                self.guest_control
                    .dir
                    .auth_release_sync_fault
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take()
            };
            if let Some(fault) = fault {
                if fault.armed.swap(false, Ordering::AcqRel) {
                    fault.fired.store(true, Ordering::Release);
                    return Err(PlanFileError::PublicationFailed);
                }
            }
        }
        dir.sync_all()
            .map_err(|_| PlanFileError::PublicationFailed)?;
        // The real bootstrap may already have unlinked this permission. Only
        // this held descriptor can validate it after consumption; the monitor
        // still requires the actual matching guest ACK before proceeding.
        verify_auth_release_name(dir, "release.json", &file, original, &bytes, true)?;
        revalidate_mount_source(self).map_err(|_| PlanFileError::ExistingOrUnsafe)?;
        Ok(())
    }

    pub(crate) fn validated_manifests(&self) -> &BTreeMap<String, Vec<u8>> {
        self.validated_manifests.as_ref()
    }

    pub(crate) fn read_guest_record(
        &self,
        name: &'static str,
        maximum: u64,
    ) -> Result<Option<Vec<u8>>, StartupGateError> {
        revalidate_mount_source(self)?;
        let file = match openat_readonly(&self.guest_control.dir.dir, name, false) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(io(error)),
        };
        validate_regular_metadata(
            &file.metadata().map_err(io)?,
            0o600,
            maximum,
            self.guest_control.dir.identity.owner_uid,
        )?;
        let bytes = read_bounded(file, maximum)?;
        revalidate_mount_source(self)?;
        Ok(Some(bytes))
    }

    #[cfg(test)]
    pub(crate) fn publish_guest_record(
        &self,
        target: &'static str,
        prefix: &'static str,
        bytes: &[u8],
    ) -> Result<PinnedRegularFile, PlanFileError> {
        revalidate_mount_source(self).map_err(|_| PlanFileError::ExistingOrUnsafe)?;
        match openat_readonly(&self.guest_control.dir.dir, target, false) {
            Ok(_) => return Err(PlanFileError::ExistingOrUnsafe),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(PlanFileError::ExistingOrUnsafe),
        }
        let guest = PinnedControlDir(Arc::clone(&self.guest_control.dir));
        guest.publish_record(target, prefix, bytes)
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

// Auth permissions are consumed by unlink, unlike durable control records.
// Keep that exception local: ordinary pinned records still require one link.
fn validate_auth_release_fd(
    file: &File,
    original: NativeFileIdentity,
    expected: &[u8],
) -> Result<u64, PlanFileError> {
    let check = |metadata: &std::fs::Metadata| {
        if !metadata.file_type().is_file()
            || identity(metadata) != original
            || metadata.nlink() > 1
            || usize::try_from(metadata.len()).ok() != Some(expected.len())
        {
            return Err(PlanFileError::ExistingOrUnsafe);
        }
        Ok(metadata.nlink())
    };
    let before = check(
        &file
            .metadata()
            .map_err(|_| PlanFileError::PublicationFailed)?,
    )?;
    let actual = read_bounded_at(file, expected.len() as u64)
        .map_err(|_| PlanFileError::PublicationFailed)?;
    let after = check(
        &file
            .metadata()
            .map_err(|_| PlanFileError::PublicationFailed)?,
    )?;
    if actual != expected || after > before {
        return Err(PlanFileError::ExistingOrUnsafe);
    }
    Ok(after)
}

fn verify_auth_release_name(
    dir: &File,
    name: &str,
    held: &File,
    original: NativeFileIdentity,
    expected: &[u8],
    allow_consumed: bool,
) -> Result<(), PlanFileError> {
    // openat_readonly is NONBLOCK|NOFOLLOW; validate regular-file metadata
    // before bounded reads so a substituted FIFO/device cannot block us.
    let named_links = match openat_readonly(dir, name, false) {
        Ok(named) => Some(validate_auth_release_fd(&named, original, expected)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => return Err(PlanFileError::ExistingOrUnsafe),
    };
    match validate_auth_release_fd(held, original, expected)? {
        1 if named_links == Some(1) => Ok(()),
        0 if allow_consumed => match openat_readonly(dir, name, false) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Ok(_) | Err(_) => Err(PlanFileError::ExistingOrUnsafe),
        },
        _ => Err(PlanFileError::ExistingOrUnsafe),
    }
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
            #[cfg(test)]
            fail_launch_plan_sync: Arc::new(AtomicBool::new(false)),
            #[cfg(test)]
            after_launch_plan_publication: std::sync::Mutex::new(None),
            #[cfg(all(test, unix))]
            after_auth_release_rename: std::sync::Mutex::new(None),
            #[cfg(all(test, unix))]
            auth_release_sync_fault: std::sync::Mutex::new(None),
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
            #[cfg(test)]
            fail_launch_plan_sync: Arc::new(AtomicBool::new(false)),
            #[cfg(test)]
            after_launch_plan_publication: std::sync::Mutex::new(None),
            #[cfg(all(test, unix))]
            after_auth_release_rename: std::sync::Mutex::new(None),
            #[cfg(all(test, unix))]
            auth_release_sync_fault: std::sync::Mutex::new(None),
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

fn read_bounded_at(file: &File, max: u64) -> Result<Vec<u8>, StartupGateError> {
    let capacity = usize::try_from(max)
        .ok()
        .and_then(|value| value.checked_add(1))
        .ok_or_else(|| invalid("oversized startup gate file"))?;
    let mut raw = vec![0; capacity];
    let mut offset = 0;
    while offset < raw.len() {
        let position = u64::try_from(offset).map_err(|_| invalid("oversized startup gate file"))?;
        let count = file.read_at(&mut raw[offset..], position).map_err(io)?;
        if count == 0 {
            raw.truncate(offset);
            return Ok(raw);
        }
        offset += count;
    }
    Err(invalid("oversized startup gate file"))
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
        for (index, pair) in intent
            .launch_token
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .enumerate()
        {
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

fn valid_request_schema(request: &StartupGateRequest, raw: &[u8]) -> bool {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(raw) else {
        return false;
    };
    let guest_cwd_present = value
        .as_object()
        .is_some_and(|fields| fields.contains_key("guest_cwd"));
    !request.bindings.is_empty()
        && match request.version {
            1 => !guest_cwd_present && request.guest_cwd.is_none(),
            2 => guest_cwd_present && request.guest_cwd.as_deref() == Some("/workspace"),
            _ => false,
        }
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
                | "cleanup.json"
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
    for stale in [
        "ready.json",
        "release.json",
        "failure.json",
        ".released",
        "cleanup.json",
    ] {
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
    if !valid_request_schema(&request, &raw) {
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
    if !valid_request_schema(&request, &raw) {
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

type LaunchPlanPublicationResult = Result<
    (PinnedControlDir, PinnedRegularFile),
    (PlanFileError, Option<(PinnedControlDir, PinnedRegularFile)>),
>;

impl PinnedControlDir {
    fn open_pinned_record(
        &self,
        target: &'static str,
    ) -> Result<Option<PinnedRegularFile>, PlanFileError> {
        match openat_readonly(&self.0.dir, target, false) {
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
                let bytes = read_bounded_at(&file, MAX_REQUEST)
                    .map_err(|_| PlanFileError::ExistingOrUnsafe)?;
                let after_metadata = file
                    .metadata()
                    .map_err(|_| PlanFileError::ExistingOrUnsafe)?;
                let after = validate_regular_metadata(
                    &after_metadata,
                    0o600,
                    MAX_REQUEST,
                    self.0.identity.owner_uid,
                )
                .map_err(|_| PlanFileError::ExistingOrUnsafe)?;
                if usize::try_from(metadata.len()).ok() != Some(bytes.len())
                    || usize::try_from(after_metadata.len()).ok() != Some(bytes.len())
                    || after != identity
                {
                    return Err(PlanFileError::ExistingOrUnsafe);
                }
                Ok(Some(PinnedRegularFile(Arc::new(PinnedRegularFileInner {
                    file,
                    identity,
                    bytes: Arc::new(bytes),
                }))))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err(PlanFileError::ExistingOrUnsafe),
        }
    }

    pub(crate) fn existing_launch_plan(&self) -> Result<Option<PinnedRegularFile>, PlanFileError> {
        self.open_pinned_record(PLAN_FILE)
    }

    pub(crate) fn publish_launch_plan_owned(&self, bytes: &[u8]) -> LaunchPlanPublicationResult {
        match self.existing_launch_plan() {
            Ok(None) => {}
            Ok(Some(_)) | Err(PlanFileError::ExistingOrUnsafe) => {
                return Err((PlanFileError::ExistingOrUnsafe, None));
            }
            Err(PlanFileError::PublicationFailed) => {
                return Err((PlanFileError::PublicationFailed, None));
            }
        }
        if bytes.len() as u64 > MAX_REQUEST {
            return Err((PlanFileError::PublicationFailed, None));
        }

        let temp_name = format!(".launch-plan.{}.tmp", uuid::Uuid::new_v4().simple());
        let fd = nix::fcntl::openat(
            &self.0.dir,
            temp_name.as_str(),
            nix::fcntl::OFlag::O_RDWR
                | nix::fcntl::OFlag::O_CREAT
                | nix::fcntl::OFlag::O_EXCL
                | nix::fcntl::OFlag::O_NOFOLLOW
                | nix::fcntl::OFlag::O_CLOEXEC,
            nix::sys::stat::Mode::from_bits_truncate(0o600),
        )
        .map_err(|_| (PlanFileError::PublicationFailed, None))?;
        let mut file = File::from(fd);
        let prepared = (|| {
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
            let metadata = file
                .metadata()
                .map_err(|_| PlanFileError::PublicationFailed)?;
            let identity =
                validate_regular_metadata(&metadata, 0o600, MAX_REQUEST, self.0.identity.owner_uid)
                    .map_err(|_| PlanFileError::PublicationFailed)?;
            if usize::try_from(metadata.len()).ok() != Some(bytes.len()) {
                return Err(PlanFileError::PublicationFailed);
            }
            Ok(PinnedRegularFile(Arc::new(PinnedRegularFileInner {
                file,
                identity,
                bytes: Arc::new(bytes.to_vec()),
            })))
        })();
        let pinned = match prepared {
            Ok(pinned) => pinned,
            Err(source) => {
                let _ = nix::unistd::unlinkat(
                    &self.0.dir,
                    temp_name.as_str(),
                    nix::unistd::UnlinkatFlags::NoRemoveDir,
                );
                return Err((source, None));
            }
        };
        match rename_noreplace(&self.0.dir, temp_name.as_str(), PLAN_FILE) {
            Ok(()) => {}
            Err(PlanFileError::ExistingOrUnsafe) => {
                let _ = nix::unistd::unlinkat(
                    &self.0.dir,
                    temp_name.as_str(),
                    nix::unistd::UnlinkatFlags::NoRemoveDir,
                );
                return Err((PlanFileError::ExistingOrUnsafe, None));
            }
            Err(source @ PlanFileError::PublicationFailed) => {
                let _ = nix::unistd::unlinkat(
                    &self.0.dir,
                    temp_name.as_str(),
                    nix::unistd::UnlinkatFlags::NoRemoveDir,
                );
                return Err((source, Some((self.clone(), pinned))));
            }
        }
        match self.sync_and_verify_launch_plan_attempt(&pinned) {
            Ok(()) => {
                #[cfg(test)]
                {
                    let action = self
                        .0
                        .after_launch_plan_publication
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .take();
                    if let Some(action) = action {
                        action();
                    }
                }
                Ok((self.clone(), pinned))
            }
            Err(source) => Err((source, Some((self.clone(), pinned)))),
        }
    }

    pub(crate) fn publish_launch_receipt(
        &self,
        bytes: &[u8],
    ) -> Result<PinnedRegularFile, PlanFileError> {
        match openat_readonly(&self.0.dir, RECEIPT_FILE, false) {
            Ok(_) => return Err(PlanFileError::ExistingOrUnsafe),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(PlanFileError::ExistingOrUnsafe),
        }
        self.publish_record(RECEIPT_FILE, ".launch", bytes)
    }

    fn sync_and_verify_record(
        &self,
        target: &'static str,
        existing: PinnedRegularFile,
        bytes: &[u8],
    ) -> Result<PinnedRegularFile, PlanFileError> {
        if existing.exact_bytes() != bytes || !existing.held_bytes_match() {
            return Err(PlanFileError::ExistingOrUnsafe);
        }
        let parent_before = validate_directory_metadata(
            &self
                .0
                .dir
                .metadata()
                .map_err(|_| PlanFileError::ExistingOrUnsafe)?,
        )
        .map_err(|_| PlanFileError::ExistingOrUnsafe)?;
        if parent_before != self.0.identity {
            return Err(PlanFileError::ExistingOrUnsafe);
        }
        existing
            .0
            .file
            .sync_all()
            .map_err(|_| PlanFileError::PublicationFailed)?;
        self.0
            .dir
            .sync_all()
            .map_err(|_| PlanFileError::PublicationFailed)?;
        let parent_after = validate_directory_metadata(
            &self
                .0
                .dir
                .metadata()
                .map_err(|_| PlanFileError::ExistingOrUnsafe)?,
        )
        .map_err(|_| PlanFileError::ExistingOrUnsafe)?;
        if parent_after != self.0.identity {
            return Err(PlanFileError::ExistingOrUnsafe);
        }
        match self.open_pinned_record(target)? {
            Some(actual)
                if actual == existing
                    && actual.exact_bytes() == bytes
                    && actual.held_bytes_match() =>
            {
                Ok(actual)
            }
            Some(_) | None => Err(PlanFileError::ExistingOrUnsafe),
        }
    }

    fn sync_and_verify_launch_plan_attempt(
        &self,
        existing: &PinnedRegularFile,
    ) -> Result<(), PlanFileError> {
        if !existing.held_bytes_match() {
            return Err(PlanFileError::ExistingOrUnsafe);
        }
        let parent_before = validate_directory_metadata(
            &self
                .0
                .dir
                .metadata()
                .map_err(|_| PlanFileError::ExistingOrUnsafe)?,
        )
        .map_err(|_| PlanFileError::ExistingOrUnsafe)?;
        if parent_before != self.0.identity {
            return Err(PlanFileError::ExistingOrUnsafe);
        }
        existing
            .0
            .file
            .sync_all()
            .map_err(|_| PlanFileError::PublicationFailed)?;
        #[cfg(test)]
        if self.0.fail_launch_plan_sync.load(Ordering::Acquire) {
            return Err(PlanFileError::PublicationFailed);
        }
        self.0
            .dir
            .sync_all()
            .map_err(|_| PlanFileError::PublicationFailed)?;
        let parent_after = validate_directory_metadata(
            &self
                .0
                .dir
                .metadata()
                .map_err(|_| PlanFileError::ExistingOrUnsafe)?,
        )
        .map_err(|_| PlanFileError::ExistingOrUnsafe)?;
        if parent_after != self.0.identity || !self.verify_launch_plan(existing) {
            return Err(PlanFileError::ExistingOrUnsafe);
        }
        Ok(())
    }

    pub(crate) fn verify_pending_launch_plan(
        &self,
        existing: &PinnedRegularFile,
    ) -> Result<(), PlanFileError> {
        self.sync_and_verify_launch_plan_attempt(existing)
    }

    pub(crate) fn publish_or_verify_cleanup_record(
        &self,
        bytes: &[u8],
    ) -> Result<PinnedRegularFile, PlanFileError> {
        match self.open_pinned_record(CLEANUP_FILE)? {
            Some(existing) if existing.exact_bytes() == bytes => {
                return self.sync_and_verify_record(CLEANUP_FILE, existing, bytes);
            }
            Some(_) => return Err(PlanFileError::ExistingOrUnsafe),
            None => {}
        }
        match self.publish_record(CLEANUP_FILE, ".cleanup", bytes) {
            Ok(pinned) => self.sync_and_verify_record(CLEANUP_FILE, pinned, bytes),
            Err(PlanFileError::ExistingOrUnsafe) => match self.open_pinned_record(CLEANUP_FILE)? {
                Some(existing) if existing.exact_bytes() == bytes => {
                    self.sync_and_verify_record(CLEANUP_FILE, existing, bytes)
                }
                Some(_) | None => Err(PlanFileError::ExistingOrUnsafe),
            },
            Err(error) => Err(error),
        }
    }

    pub(crate) fn verify_cleanup_record(&self, expected: &PinnedRegularFile) -> bool {
        expected.held_bytes_match()
            && matches!(
                self.open_pinned_record(CLEANUP_FILE),
                Ok(Some(actual)) if actual == *expected
            )
    }

    fn publish_record(
        &self,
        target: &'static str,
        prefix: &'static str,
        bytes: &[u8],
    ) -> Result<PinnedRegularFile, PlanFileError> {
        if bytes.len() as u64 > MAX_REQUEST {
            return Err(PlanFileError::PublicationFailed);
        }
        let temp_name = format!("{prefix}.{}.tmp", uuid::Uuid::new_v4().simple());
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
            rename_noreplace(&self.0.dir, temp_name.as_str(), target)?;
            #[cfg(test)]
            if target == PLAN_FILE && self.0.fail_launch_plan_sync.load(Ordering::Acquire) {
                return Err(PlanFileError::PublicationFailed);
            }
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
            let Some(pinned) = self
                .open_pinned_record(target)
                .map_err(|_| PlanFileError::PublicationFailed)?
            else {
                return Err(PlanFileError::PublicationFailed);
            };
            if pinned.0.identity != identity
                || pinned.exact_bytes() != bytes
                || !pinned.held_bytes_match()
            {
                return Err(PlanFileError::PublicationFailed);
            }
            Ok(pinned)
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
        expected.held_bytes_match()
            && matches!(self.existing_launch_plan(), Ok(Some(actual)) if actual == *expected)
    }

    pub(crate) fn verify_launch_receipt(&self, expected: &PinnedRegularFile) -> bool {
        expected.held_bytes_match()
            && matches!(
                self.open_pinned_record(RECEIPT_FILE),
                Ok(Some(actual)) if actual == *expected
            )
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
