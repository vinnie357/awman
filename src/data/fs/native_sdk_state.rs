//! Private native SDK state acquisition; execution custody and ACK publication are separate.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::data::error::DataError;
use crate::data::fs::data_paths::DataPaths;

const SCHEMA_VERSION: u32 = 1;
const MAX_SESSION_ID_BYTES: usize = 1024;
const MAX_RECORD_BYTES: u64 = 65536;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum NativeSdkProvider {
    Claude,
    Codex,
    Agy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeFileIdentity {
    device: u64,
    inode: u64,
    owner_uid: u32,
    mode: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeSdkStateKey {
    project: PathBuf,
    project_identity: NativeFileIdentity,
    provider: NativeSdkProvider,
    model: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnerRecord {
    schema_version: u32,
    generation: Uuid,
    key: NativeSdkStateKey,
    data_root: NativeFileIdentity,
    store: NativeFileIdentity,
    namespace: NativeFileIdentity,
    owner_dir: NativeFileIdentity,
    state_dir: NativeFileIdentity,
    writer_lock: NativeFileIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AssociationRecord {
    schema_version: u32,
    session_id: String,
    owner: OwnerRecord,
}

pub(crate) struct NativeSdkState {
    #[cfg(unix)]
    root: std::sync::Arc<unix::PinnedDir>,
    #[cfg(unix)]
    store: std::sync::Arc<unix::PinnedDir>,
}

pub(crate) struct NativeSdkStateLease {
    owner_record: OwnerRecord,
    owner_path: PathBuf,
    state_path: PathBuf,
    acquired_session: Option<String>,
    #[cfg(unix)]
    directories: Vec<std::sync::Arc<unix::PinnedDir>>,
    #[cfg(unix)]
    writer_lock: std::fs::File,
}

const ACTIVE_RUN: &str = "active-run.json";
const COMPLETED_RUN: &str = "completed-run.json";

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActiveRunRecord {
    schema_version: u32,
    run_id: Uuid,
    owner: OwnerRecord,
    #[serde(deserialize_with = "required_requested_session")]
    requested_session: Option<String>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CompletedRunRecord {
    schema_version: u32,
    run_id: Uuid,
    owner: OwnerRecord,
    #[serde(deserialize_with = "required_requested_session")]
    requested_session: Option<String>,
    session_id: String,
}

fn required_requested_session<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer)
}

impl ActiveRunRecord {
    fn validate(&self, owner: &OwnerRecord) -> Result<(), DataError> {
        if self.schema_version != SCHEMA_VERSION || self.owner != *owner {
            return Err(invalid("active run does not match the acquired owner"));
        }
        if let Some(session) = &self.requested_session {
            validate_session_id(session)?;
        }
        Ok(())
    }
}

impl CompletedRunRecord {
    fn new(active: &ActiveRunRecord, session: &str) -> Self {
        Self {
            schema_version: active.schema_version,
            run_id: active.run_id,
            owner: active.owner.clone(),
            requested_session: active.requested_session.clone(),
            session_id: session.into(),
        }
    }

    fn validate(&self, owner: &OwnerRecord) -> Result<(), DataError> {
        if self.schema_version != SCHEMA_VERSION || self.owner != *owner {
            return Err(invalid("completed run does not match the acquired owner"));
        }
        validate_session_id(&self.session_id)?;
        if let Some(session) = &self.requested_session {
            validate_session_id(session)?;
            if session != &self.session_id {
                return Err(invalid(
                    "completed acknowledgement differs from its request",
                ));
            }
        }
        Ok(())
    }

    fn matches(&self, active: &ActiveRunRecord) -> bool {
        self.schema_version == active.schema_version
            && self.run_id == active.run_id
            && self.owner == active.owner
            && self.requested_session == active.requested_session
    }
}

enum NativeSdkRunPhase {
    Active,
    PublicationUncertain(Box<CompletedRunRecord>),
    Completed,
}

pub(crate) struct NativeSdkRunLease {
    lease: NativeSdkStateLease,
    activation: ActiveRunRecord,
    phase: NativeSdkRunPhase,
}

pub(crate) enum NativeSdkRunCompletion {
    Completed,
    CompletedPublicationUncertain(DataError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)] // The reviewed fault API names each post-operation boundary.
pub(crate) enum NativeSdkRunFsFault {
    AfterActivationRename,
    AfterAssociationRename,
    AfterCompletionRename,
    AfterCompletionDirectorySync,
}

impl NativeSdkState {
    pub(crate) fn acquire_first_run(
        &self,
        key: &NativeSdkStateKey,
    ) -> Result<NativeSdkRunLease, DataError> {
        self.acquire_first_run_with_fault(key, None)
    }

    #[cfg(test)]
    pub(crate) fn acquire_resume_run(
        &self,
        key: &NativeSdkStateKey,
        session: &str,
    ) -> Result<NativeSdkRunLease, DataError> {
        self.acquire_resume_run_with_fault(key, session, None)
    }
}

impl NativeSdkRunLease {
    pub(crate) fn state_path(&self) -> &Path {
        self.lease.state_path()
    }

    pub(crate) fn validate_request(
        &self,
        provider: NativeSdkProvider,
        model: &str,
        session: Option<&str>,
    ) -> Result<(), DataError> {
        self.lease.validate_request(provider, model, session)?;
        self.validate_active()
    }

    pub(crate) fn complete_run(
        &mut self,
        session: &str,
    ) -> Result<NativeSdkRunCompletion, DataError> {
        self.complete_run_with_fault(session, None)
    }

    /// Create first-owner config exclusively under the pinned state descriptor.
    #[cfg(unix)]
    pub(crate) fn create_codex_auth_config(&self) -> Result<(), DataError> {
        self.validate_active()?;
        self.lease.create_codex_auth_config()?;
        self.validate_active()
    }

    #[cfg(not(unix))]
    pub(crate) fn create_codex_auth_config(&self) -> Result<(), DataError> {
        Err(invalid("native Codex authentication requires Unix"))
    }

    pub(crate) fn reconcile_completed(&mut self) -> Result<NativeSdkRunCompletion, DataError> {
        self.reconcile_completed_with_fault(None)
    }
}

impl NativeSdkStateLease {
    pub(crate) fn state_path(&self) -> &Path {
        &self.state_path
    }

    pub(crate) fn validate_request(
        &self,
        provider: NativeSdkProvider,
        model: &str,
        session: Option<&str>,
    ) -> Result<(), DataError> {
        self.validate()?;
        if self.owner_record.key.provider != provider
            || self.owner_record.key.model != model
            || self.acquired_session.as_deref() != session
        {
            return Err(invalid("request does not match the acquired state owner"));
        }
        Ok(())
    }
}

#[cfg(unix)]
impl Drop for NativeSdkStateLease {
    fn drop(&mut self) {
        // Fork-inherited descriptors can outlive this lease, so release its writer authority now.
        if let Err(error) = self.writer_lock.unlock() {
            tracing::warn!(%error, "cannot unlock native SDK state writer lease");
        }
    }
}

fn invalid(reason: &str) -> DataError {
    DataError::Other(format!("native SDK state: {reason}"))
}

fn validate_session_id(session_id: &str) -> Result<(), DataError> {
    if session_id.is_empty()
        || session_id.trim() != session_id
        || session_id.len() > MAX_SESSION_ID_BYTES
        || session_id.chars().any(char::is_control)
    {
        return Err(invalid("invalid requested session identifier"));
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::fs::{File, Metadata, OpenOptions};
    use std::io::{Read, Write};
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    use std::sync::Arc;

    use nix::fcntl::{openat, OFlag};
    use nix::sys::stat::{mkdirat, Mode};

    pub(super) struct PinnedDir {
        file: File,
        path: PathBuf,
        identity: NativeFileIdentity,
        private: bool,
    }

    fn identity(metadata: &Metadata) -> NativeFileIdentity {
        NativeFileIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
            owner_uid: metadata.uid(),
            mode: metadata.mode() & 0o7777,
        }
    }

    fn directory_identity(
        metadata: &Metadata,
        private: bool,
    ) -> Result<NativeFileIdentity, DataError> {
        let id = identity(metadata);
        if !metadata.is_dir()
            || id.owner_uid != nix::unistd::geteuid().as_raw()
            || id.mode & 0o022 != 0
            || (private && id.mode != 0o700)
        {
            return Err(invalid("unsafe directory ownership or permissions"));
        }
        Ok(id)
    }

    fn regular_identity(file: &File, path: &Path) -> Result<NativeFileIdentity, DataError> {
        let metadata = file
            .metadata()
            .map_err(|error| DataError::io(path, error))?;
        let id = identity(&metadata);
        if !metadata.is_file()
            || id.owner_uid != nix::unistd::geteuid().as_raw()
            || id.mode != 0o600
            || metadata.nlink() != 1
            || metadata.len() > MAX_RECORD_BYTES
        {
            return Err(invalid("unsafe owner record or writer lock"));
        }
        Ok(id)
    }

    impl PinnedDir {
        fn at(path: &Path, private: bool) -> Result<Arc<Self>, DataError> {
            let supplied = std::fs::symlink_metadata(path).map_err(|e| DataError::io(path, e))?;
            let expected = directory_identity(&supplied, private)?;
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(
                    libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
                )
                .open(path)
                .map_err(|error| DataError::io(path, error))?;
            let id = directory_identity(
                &file.metadata().map_err(|e| DataError::io(path, e))?,
                private,
            )?;
            let current = std::fs::symlink_metadata(path).map_err(|e| DataError::io(path, e))?;
            if id != expected || directory_identity(&current, private)? != id {
                return Err(invalid("directory changed while acquiring its descriptor"));
            }
            let canonical = std::fs::canonicalize(path).map_err(|e| DataError::io(path, e))?;
            let dir = Arc::new(Self {
                file,
                path: canonical,
                identity: id,
                private,
            });
            dir.validate()?;
            Ok(dir)
        }

        fn child(&self, name: &str, create: bool) -> Result<Arc<Self>, DataError> {
            let path = self.path.join(name);
            if create {
                match mkdirat(&self.file, name, Mode::from_bits_truncate(0o700)) {
                    Ok(()) | Err(nix::errno::Errno::EEXIST) => {}
                    Err(error) => return Err(DataError::io(&path, error.into())),
                }
            }
            let file = open_relative(self, name, OFlag::O_RDONLY | OFlag::O_DIRECTORY)?;
            let id =
                directory_identity(&file.metadata().map_err(|e| DataError::io(&path, e))?, true)?;
            let dir = Arc::new(Self {
                file,
                path,
                identity: id,
                private: true,
            });
            dir.validate()?;
            Ok(dir)
        }

        fn validate(&self) -> Result<(), DataError> {
            let current =
                std::fs::symlink_metadata(&self.path).map_err(|e| DataError::io(&self.path, e))?;
            let held = self
                .file
                .metadata()
                .map_err(|e| DataError::io(&self.path, e))?;
            if directory_identity(&current, self.private)? != self.identity
                || directory_identity(&held, self.private)? != self.identity
            {
                return Err(invalid("owned directory identity changed"));
            }
            Ok(())
        }
    }

    fn open_relative(parent: &PinnedDir, name: &str, flags: OFlag) -> Result<File, DataError> {
        openat(
            &parent.file,
            name,
            flags | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_NONBLOCK,
            Mode::from_bits_truncate(0o600),
        )
        .map(File::from)
        .map_err(|e| DataError::io(parent.path.join(name), e.into()))
    }

    fn read_record<T: serde::de::DeserializeOwned>(
        parent: &PinnedDir,
        name: &str,
    ) -> Result<T, DataError> {
        let file = open_relative(parent, name, OFlag::O_RDONLY)?;
        let path = parent.path.join(name);
        let before = regular_identity(&file, &path)?;
        let mut bytes = Vec::new();
        (&file)
            .take(MAX_RECORD_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| DataError::io(&path, e))?;
        let named = open_relative(parent, name, OFlag::O_RDONLY)?;
        if bytes.len() as u64 > MAX_RECORD_BYTES
            || regular_identity(&file, &path)? != before
            || regular_identity(&named, &path)? != before
        {
            return Err(invalid("owner record changed or exceeded its bound"));
        }
        serde_json::from_slice(&bytes).map_err(|e| DataError::config_parse(path, e))
    }

    impl NativeSdkStateKey {
        pub(crate) fn new(
            project: &Path,
            provider: NativeSdkProvider,
            model: &str,
        ) -> Result<Self, DataError> {
            if model.trim().is_empty() {
                return Err(invalid("an explicit nonempty model is required"));
            }
            let project = PinnedDir::at(project, false)?;
            Ok(Self {
                project: project.path.clone(),
                project_identity: project.identity.clone(),
                provider,
                model: model.into(),
            })
        }
    }

    impl NativeSdkState {
        pub(crate) fn new(paths: &DataPaths) -> Result<Self, DataError> {
            let root = PinnedDir::at(paths.root(), false)?;
            let store = root.child("native-sdk-state", true)?;
            root.validate()?;
            Ok(Self { root, store })
        }

        pub(super) fn namespace_path(&self, key: &NativeSdkStateKey) -> PathBuf {
            let project = key.project.as_os_str().as_encoded_bytes();
            let provider = match key.provider {
                NativeSdkProvider::Claude => "claude",
                NativeSdkProvider::Codex => "codex",
                NativeSdkProvider::Agy => "agy",
            };
            let mut hash = Sha256::new();
            hash.update((project.len() as u64).to_le_bytes());
            hash.update(project);
            hash.update(provider.as_bytes());
            self.store.path.join(hex(&hash.finalize()))
        }

        #[cfg(test)]
        pub(super) fn association_path(
            &self,
            key: &NativeSdkStateKey,
            session_id: &str,
        ) -> Result<PathBuf, DataError> {
            validate_session_id(session_id)?;
            Ok(self.namespace_path(key).join("sessions").join(format!(
                "{}.json",
                hex(&Sha256::digest(session_id.as_bytes()))
            )))
        }

        fn acquire_directories(
            &self,
            key: &NativeSdkStateKey,
            create: bool,
        ) -> Result<Vec<Arc<PinnedDir>>, DataError> {
            self.root.validate()?;
            self.store.validate()?;
            let project = PinnedDir::at(&key.project, false)?;
            if project.identity != key.project_identity {
                return Err(invalid("project identity changed"));
            }
            let namespace_path = self.namespace_path(key);
            if self.store.path.starts_with(&project.path)
                || project.path.starts_with(&self.store.path)
                || namespace_path.starts_with(&project.path)
                || project.path.starts_with(&namespace_path)
            {
                return Err(invalid("native SDK state overlaps the project"));
            }
            let name = namespace_path
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or_else(|| invalid("invalid namespace"))?;
            let namespace = self.store.child(name, create)?;
            let owners = namespace.child("owners", create)?;
            let sessions = namespace.child("sessions", create)?;
            Ok(vec![
                project,
                self.root.clone(),
                self.store.clone(),
                namespace,
                owners,
                sessions,
            ])
        }

        pub(crate) fn acquire_first(
            &self,
            key: &NativeSdkStateKey,
        ) -> Result<NativeSdkStateLease, DataError> {
            let mut directories = self.acquire_directories(key, true)?;
            let generation = Uuid::new_v4();
            let name = generation.to_string();
            let owners = &directories[4];
            mkdirat(&owners.file, name.as_str(), Mode::from_bits_truncate(0o700))
                .map_err(|e| DataError::io(owners.path.join(&name), e.into()))?;
            let owner = owners.child(&name, false)?;
            let state = owner.child("state", true)?;
            let writer_lock = open_relative(
                &owner,
                "writer.lock",
                OFlag::O_RDWR | OFlag::O_CREAT | OFlag::O_EXCL,
            )?;
            lock_writer(&writer_lock)?;
            let owner_record = OwnerRecord {
                schema_version: SCHEMA_VERSION,
                generation,
                key: key.clone(),
                data_root: self.root.identity.clone(),
                store: self.store.identity.clone(),
                namespace: directories[3].identity.clone(),
                owner_dir: owner.identity.clone(),
                state_dir: state.identity.clone(),
                writer_lock: regular_identity(&writer_lock, &owner.path.join("writer.lock"))?,
            };
            let bytes = serde_json::to_vec(&owner_record)
                .map_err(|source| DataError::ConfigSerialize { source })?;
            if bytes.len() as u64 > MAX_RECORD_BYTES {
                return Err(invalid("owner record exceeded its bound"));
            }
            let mut file = open_relative(
                &owner,
                "owner.json",
                OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL,
            )?;
            file.write_all(&bytes)
                .and_then(|()| file.sync_all())
                .map_err(|e| DataError::io(owner.path.join("owner.json"), e))?;
            owner
                .file
                .sync_all()
                .map_err(|e| DataError::io(&owner.path, e))?;
            let owner_path = owner.path.clone();
            let state_path = state.path.clone();
            directories.extend([owner, state]);
            let lease = NativeSdkStateLease {
                owner_record,
                owner_path,
                state_path,
                acquired_session: None,
                directories,
                writer_lock,
            };
            lease.validate()?;
            Ok(lease)
        }

        #[cfg(test)]
        pub(crate) fn acquire_resume(
            &self,
            key: &NativeSdkStateKey,
            session_id: &str,
        ) -> Result<NativeSdkStateLease, DataError> {
            let association_path = self.association_path(key, session_id)?;
            let mut directories = self.acquire_directories(key, false)?;
            let name = association_path
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or_else(|| invalid("invalid session filename"))?;
            let association: AssociationRecord = read_record(&directories[5], name)?;
            let record = association.owner;
            if association.schema_version != SCHEMA_VERSION
                || association.session_id != session_id
                || record.schema_version != SCHEMA_VERSION
                || record.key != *key
                || record.data_root != self.root.identity
                || record.store != self.store.identity
                || record.namespace != directories[3].identity
            {
                return Err(invalid(
                    "missing or foreign acknowledged native state association",
                ));
            }
            let owner = directories[4].child(&record.generation.to_string(), false)?;
            let state = owner.child("state", false)?;
            let writer_lock = open_relative(&owner, "writer.lock", OFlag::O_RDWR)?;
            if owner.identity != record.owner_dir
                || state.identity != record.state_dir
                || regular_identity(&writer_lock, &owner.path.join("writer.lock"))?
                    != record.writer_lock
                || read_record::<OwnerRecord>(&owner, "owner.json")? != record
            {
                return Err(invalid("acknowledged owner identity changed"));
            }
            lock_writer(&writer_lock)?;
            let owner_path = owner.path.clone();
            let state_path = state.path.clone();
            directories.extend([owner, state]);
            let lease = NativeSdkStateLease {
                owner_record: record,
                owner_path,
                state_path,
                acquired_session: Some(session_id.into()),
                directories,
                writer_lock,
            };
            lease.validate()?;
            Ok(lease)
        }
    }

    fn run_fault(
        fault: Option<NativeSdkRunFsFault>,
        boundary: NativeSdkRunFsFault,
        path: &Path,
    ) -> Result<(), DataError> {
        if fault == Some(boundary) {
            return Err(DataError::io(
                path,
                std::io::Error::other(format!("injected native SDK run-state fault: {boundary:?}")),
            ));
        }
        Ok(())
    }

    impl NativeSdkState {
        pub(crate) fn acquire_first_run_with_fault(
            &self,
            key: &NativeSdkStateKey,
            fault: Option<NativeSdkRunFsFault>,
        ) -> Result<NativeSdkRunLease, DataError> {
            NativeSdkRunLease::activate(self.acquire_first(key)?, fault)
        }

        #[cfg(test)]
        pub(crate) fn acquire_resume_run_with_fault(
            &self,
            key: &NativeSdkStateKey,
            session: &str,
            fault: Option<NativeSdkRunFsFault>,
        ) -> Result<NativeSdkRunLease, DataError> {
            let lease = self.acquire_resume(key, session)?;
            lease.read_run_proof(session)?;
            NativeSdkRunLease::activate(lease, fault)
        }
    }

    impl NativeSdkRunLease {
        pub(crate) fn validate_main_selection(
            &self,
            project: &Path,
            controls_root: &Path,
        ) -> Result<PathBuf, DataError> {
            self.validate_active()?;
            let key = NativeSdkStateKey::new(
                project,
                NativeSdkProvider::Codex,
                &self.activation.owner.key.model,
            )?;
            let root = PinnedDir::at(controls_root, false)?;
            if key != self.activation.owner.key
                || root.path != self.lease.directories[1].path
                || root.identity != self.lease.owner_record.data_root
                || root.path.starts_with(&key.project)
                || self.lease.directories[2].path.starts_with(&key.project)
                || key.project.starts_with(&self.lease.directories[2].path)
            {
                return Err(invalid("main selection differs from original owner"));
            }
            self.validate_active()?;
            Ok(key.project)
        }
        fn activate(
            lease: NativeSdkStateLease,
            fault: Option<NativeSdkRunFsFault>,
        ) -> Result<Self, DataError> {
            let activation = ActiveRunRecord {
                schema_version: SCHEMA_VERSION,
                run_id: Uuid::new_v4(),
                owner: lease.owner_record.clone(),
                requested_session: lease.acquired_session.clone(),
            };
            let run = Self {
                lease,
                activation,
                phase: NativeSdkRunPhase::Active,
            };
            let (pending, file) = run.lease.prepare_run_record(ACTIVE_RUN, &run.activation)?;
            run.lease.rename_run_record(&pending, &file, ACTIVE_RUN)?;
            run_fault(
                fault,
                NativeSdkRunFsFault::AfterActivationRename,
                &run.lease.owner_path,
            )?;
            run.lease.sync_run_directory()?;
            run.validate_active()?;
            Ok(run)
        }

        pub(super) fn validate_active(&self) -> Result<(), DataError> {
            self.lease.validate()?;
            let active: ActiveRunRecord = read_record(&self.lease.directories[6], ACTIVE_RUN)?;
            active.validate(&self.lease.owner_record)?;
            if active != self.activation
                || self.activation.requested_session != self.lease.acquired_session
            {
                return Err(invalid("named activation differs from the acquired run"));
            }
            self.lease.validate()
        }

        fn validate_completed(&self, expected: &CompletedRunRecord) -> Result<(), DataError> {
            let (active, completed) = self.lease.read_run_proof(&expected.session_id)?;
            if active != self.activation || completed != *expected {
                return Err(invalid("completed proof differs from the retained run"));
            }
            Ok(())
        }

        pub(crate) fn complete_run_with_fault(
            &mut self,
            session: &str,
            fault: Option<NativeSdkRunFsFault>,
        ) -> Result<NativeSdkRunCompletion, DataError> {
            if !matches!(self.phase, NativeSdkRunPhase::Active) {
                return Err(invalid("run completion is already published or uncertain"));
            }
            validate_session_id(session)?;
            if self
                .activation
                .requested_session
                .as_deref()
                .is_some_and(|id| id != session)
            {
                return Err(invalid("acknowledged session changed during resume"));
            }
            self.validate_active()?;
            self.lease.validate_run_destination(COMPLETED_RUN)?;
            self.lease.publish_session_with_run_fault(session, fault)?;
            self.validate_active()?;
            self.lease.sync_session_directory()?;
            self.lease.read_run_association(session)?;
            self.validate_active()?;
            let completed = CompletedRunRecord::new(&self.activation, session);
            let (pending, file) = self.lease.prepare_run_record(COMPLETED_RUN, &completed)?;
            self.validate_active()?;
            self.lease
                .rename_run_record(&pending, &file, COMPLETED_RUN)?;
            // A visible completion must retain uncertainty before any later fallible operation.
            self.phase = NativeSdkRunPhase::PublicationUncertain(Box::new(completed.clone()));
            let result = (|| {
                run_fault(
                    fault,
                    NativeSdkRunFsFault::AfterCompletionRename,
                    &self.lease.owner_path,
                )?;
                self.lease.sync_run_directory()?;
                run_fault(
                    fault,
                    NativeSdkRunFsFault::AfterCompletionDirectorySync,
                    &self.lease.owner_path,
                )?;
                self.validate_completed(&completed)
            })();
            match result {
                Ok(()) => {
                    self.phase = NativeSdkRunPhase::Completed;
                    Ok(NativeSdkRunCompletion::Completed)
                }
                Err(error) => Ok(NativeSdkRunCompletion::CompletedPublicationUncertain(error)),
            }
        }

        pub(crate) fn reconcile_completed_with_fault(
            &mut self,
            fault: Option<NativeSdkRunFsFault>,
        ) -> Result<NativeSdkRunCompletion, DataError> {
            if fault.is_some_and(|fault| fault != NativeSdkRunFsFault::AfterCompletionDirectorySync)
            {
                return Err(invalid("reconciliation has no rename fault boundary"));
            }
            let expected = match &self.phase {
                NativeSdkRunPhase::PublicationUncertain(expected) => expected.clone(),
                NativeSdkRunPhase::Active | NativeSdkRunPhase::Completed => {
                    return Err(invalid("run has no uncertain completion to reconcile"));
                }
            };
            self.validate_completed(&expected)?;
            self.lease.sync_session_directory()?;
            self.lease.sync_run_directory()?;
            run_fault(
                fault,
                NativeSdkRunFsFault::AfterCompletionDirectorySync,
                &self.lease.owner_path,
            )?;
            self.validate_completed(&expected)?;
            self.phase = NativeSdkRunPhase::Completed;
            Ok(NativeSdkRunCompletion::Completed)
        }
    }

    impl NativeSdkStateLease {
        pub(super) fn create_codex_auth_config(&self) -> Result<(), DataError> {
            self.validate()?;
            let state = self
                .directories
                .last()
                .ok_or_else(|| invalid("missing state pin"))?;
            let path = state.path.join("config.toml");
            let mut file = open_relative(
                state,
                "config.toml",
                OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL,
            )?;
            file.write_all(super::super::native_codex_auth::FRESH_CONFIG)
                .map_err(|error| DataError::io(&path, error))?;
            file.sync_all()
                .map_err(|error| DataError::io(&path, error))?;
            state
                .file
                .sync_all()
                .map_err(|error| DataError::io(&state.path, error))?;
            self.validate()
        }

        fn read_run_association(&self, session: &str) -> Result<(), DataError> {
            validate_session_id(session)?;
            let name = format!("{}.json", hex(&Sha256::digest(session.as_bytes())));
            let record: AssociationRecord = read_record(&self.directories[5], &name)?;
            if record.schema_version != SCHEMA_VERSION
                || record.session_id != session
                || record.owner != self.owner_record
            {
                return Err(invalid("association differs from the acquired run owner"));
            }
            Ok(())
        }

        fn read_run_proof(
            &self,
            session: &str,
        ) -> Result<(ActiveRunRecord, CompletedRunRecord), DataError> {
            self.validate()?;
            self.read_run_association(session)?;
            let active: ActiveRunRecord = read_record(&self.directories[6], ACTIVE_RUN)?;
            let completed: CompletedRunRecord = read_record(&self.directories[6], COMPLETED_RUN)?;
            active.validate(&self.owner_record)?;
            completed.validate(&self.owner_record)?;
            if !completed.matches(&active) || completed.session_id != session {
                return Err(invalid(
                    "latest activation has no exact matching completion",
                ));
            }
            self.validate()?;
            Ok((active, completed))
        }

        fn validate_run_destination(&self, name: &str) -> Result<(), DataError> {
            let result = if name == ACTIVE_RUN {
                read_record::<ActiveRunRecord>(&self.directories[6], name)
                    .and_then(|record| record.validate(&self.owner_record))
            } else {
                read_record::<CompletedRunRecord>(&self.directories[6], name)
                    .and_then(|record| record.validate(&self.owner_record))
            };
            match result {
                Err(DataError::Io { source, .. })
                    if source.kind() == std::io::ErrorKind::NotFound =>
                {
                    Ok(())
                }
                result => result,
            }
        }

        fn prepare_run_record<T: Serialize>(
            &self,
            name: &str,
            record: &T,
        ) -> Result<(String, File), DataError> {
            self.validate()?;
            self.validate_run_destination(name)?;
            let bytes = serde_json::to_vec(record)
                .map_err(|source| DataError::ConfigSerialize { source })?;
            if bytes.len() as u64 > MAX_RECORD_BYTES {
                return Err(invalid("run record exceeded its bound"));
            }
            let owner = &self.directories[6];
            let pending = format!("run-{}.pending", Uuid::new_v4());
            let mut file = open_relative(
                owner,
                &pending,
                OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL,
            )?;
            file.write_all(&bytes)
                .and_then(|()| file.sync_all())
                .map_err(|error| DataError::io(owner.path.join(&pending), error))?;
            self.validate()?;
            Ok((pending, file))
        }

        fn rename_run_record(
            &self,
            pending: &str,
            file: &File,
            name: &str,
        ) -> Result<(), DataError> {
            self.validate()?;
            self.validate_run_destination(name)?;
            let owner = &self.directories[6];
            let named = open_relative(owner, pending, OFlag::O_RDONLY)?;
            if regular_identity(file, &owner.path.join(pending))?
                != regular_identity(&named, &owner.path.join(pending))?
            {
                return Err(invalid("pending run record identity changed"));
            }
            rustix::fs::renameat(&owner.file, pending, &owner.file, name)
                .map_err(|error| DataError::io(owner.path.join(name), error.into()))
        }

        fn sync_session_directory(&self) -> Result<(), DataError> {
            self.validate()?;
            let sessions = &self.directories[5];
            sessions
                .file
                .sync_all()
                .map_err(|error| DataError::io(&sessions.path, error))
        }

        fn sync_run_directory(&self) -> Result<(), DataError> {
            self.validate()?;
            let owner = &self.directories[6];
            owner
                .file
                .sync_all()
                .map_err(|error| DataError::io(&owner.path, error))
        }
    }

    fn lock_writer(file: &File) -> Result<(), DataError> {
        match file.try_lock() {
            Ok(()) => Ok(()),
            Err(std::fs::TryLockError::WouldBlock) => {
                Err(invalid("native state already has an active writer"))
            }
            Err(std::fs::TryLockError::Error(error)) => {
                Err(invalid(&format!("cannot lock native state: {error}")))
            }
        }
    }

    impl NativeSdkStateLease {
        #[cfg(test)]
        pub(crate) fn publish_session(&self, session_id: &str) -> Result<(), DataError> {
            self.publish_session_with_run_fault(session_id, None)
        }

        fn publish_session_with_run_fault(
            &self,
            session_id: &str,
            fault: Option<NativeSdkRunFsFault>,
        ) -> Result<(), DataError> {
            validate_session_id(session_id)?;
            self.validate()?;
            if self
                .acquired_session
                .as_deref()
                .is_some_and(|id| id != session_id)
            {
                return Err(invalid("acknowledged session changed during resume"));
            }
            let sessions = &self.directories[5];
            let owner = &self.directories[6];
            let name = format!("{}.json", hex(&Sha256::digest(session_id.as_bytes())));
            let record = AssociationRecord {
                schema_version: SCHEMA_VERSION,
                session_id: session_id.into(),
                owner: self.owner_record.clone(),
            };
            match open_relative(sessions, &name, OFlag::O_RDONLY) {
                Ok(_) => {
                    if read_record::<AssociationRecord>(sessions, &name)? != record {
                        return Err(invalid("session already belongs to another state owner"));
                    }
                    return self.validate();
                }
                Err(DataError::Io { source, .. })
                    if source.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            let bytes = serde_json::to_vec(&record)
                .map_err(|source| DataError::ConfigSerialize { source })?;
            if bytes.len() as u64 > MAX_RECORD_BYTES {
                return Err(invalid("session association exceeded its bound"));
            }
            let pending = format!("association-{}.pending", Uuid::new_v4());
            let mut file = open_relative(
                owner,
                &pending,
                OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL,
            )?;
            file.write_all(&bytes)
                .and_then(|()| file.sync_all())
                .map_err(|error| DataError::io(owner.path.join(&pending), error))?;
            self.validate()?;
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            rustix::fs::renameat_with(
                &owner.file,
                pending.as_str(),
                &sessions.file,
                name.as_str(),
                rustix::fs::RenameFlags::NOREPLACE,
            )
            .map_err(|error| DataError::io(sessions.path.join(&name), error.into()))?;
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            run_fault(
                fault,
                NativeSdkRunFsFault::AfterAssociationRename,
                &sessions.path,
            )?;
            #[cfg(not(any(target_os = "macos", target_os = "linux")))]
            return Err(invalid("atomic native session publication is unsupported"));
            sessions
                .file
                .sync_all()
                .map_err(|error| DataError::io(&sessions.path, error))?;
            self.validate()?;
            if read_record::<AssociationRecord>(sessions, &name)? != record {
                return Err(invalid("published association changed"));
            }
            Ok(())
        }

        pub(crate) fn validate(&self) -> Result<(), DataError> {
            for directory in &self.directories {
                directory.validate()?;
            }
            let owner = &self.directories[6];
            let lock = open_relative(owner, "writer.lock", OFlag::O_RDWR)?;
            if self.directories[2].identity != self.owner_record.store
                || regular_identity(&lock, &owner.path.join("writer.lock"))?
                    != self.owner_record.writer_lock
                || regular_identity(&self.writer_lock, &owner.path.join("writer.lock"))?
                    != self.owner_record.writer_lock
                || read_record::<OwnerRecord>(owner, "owner.json")? != self.owner_record
            {
                return Err(invalid("held owner record or writer lock changed"));
            }
            Ok(())
        }
    }
}

#[cfg(not(unix))]
impl NativeSdkState {
    pub(crate) fn acquire_first_run_with_fault(
        &self,
        _: &NativeSdkStateKey,
        _: Option<NativeSdkRunFsFault>,
    ) -> Result<NativeSdkRunLease, DataError> {
        Err(invalid(
            "native SDK run-state storage is unsupported on this platform",
        ))
    }

    #[cfg(test)]
    pub(crate) fn acquire_resume_run_with_fault(
        &self,
        _: &NativeSdkStateKey,
        _: &str,
        _: Option<NativeSdkRunFsFault>,
    ) -> Result<NativeSdkRunLease, DataError> {
        Err(invalid(
            "native SDK run-state storage is unsupported on this platform",
        ))
    }
}

#[cfg(not(unix))]
impl NativeSdkRunLease {
    pub(crate) fn validate_main_selection(&self, _: &Path, _: &Path) -> Result<PathBuf, DataError> {
        Err(invalid("native SDK main requires Unix"))
    }
    fn validate_active(&self) -> Result<(), DataError> {
        Err(invalid(
            "native SDK run-state storage is unsupported on this platform",
        ))
    }

    pub(crate) fn complete_run_with_fault(
        &mut self,
        _: &str,
        _: Option<NativeSdkRunFsFault>,
    ) -> Result<NativeSdkRunCompletion, DataError> {
        Err(invalid(
            "native SDK run-state storage is unsupported on this platform",
        ))
    }

    pub(crate) fn reconcile_completed_with_fault(
        &mut self,
        _: Option<NativeSdkRunFsFault>,
    ) -> Result<NativeSdkRunCompletion, DataError> {
        Err(invalid(
            "native SDK run-state storage is unsupported on this platform",
        ))
    }
}

#[cfg(not(unix))]
impl NativeSdkStateKey {
    pub(crate) fn new(_: &Path, _: NativeSdkProvider, _: &str) -> Result<Self, DataError> {
        Err(invalid(
            "native SDK state acquisition is unsupported on this platform",
        ))
    }
}

#[cfg(not(unix))]
impl NativeSdkState {
    pub(crate) fn new(_: &DataPaths) -> Result<Self, DataError> {
        Err(invalid(
            "native SDK state acquisition is unsupported on this platform",
        ))
    }
    pub(crate) fn acquire_first(
        &self,
        _: &NativeSdkStateKey,
    ) -> Result<NativeSdkStateLease, DataError> {
        Err(invalid(
            "native SDK state acquisition is unsupported on this platform",
        ))
    }
    #[cfg(test)]
    pub(crate) fn acquire_resume(
        &self,
        _: &NativeSdkStateKey,
        _: &str,
    ) -> Result<NativeSdkStateLease, DataError> {
        Err(invalid(
            "native SDK state acquisition is unsupported on this platform",
        ))
    }
}

#[cfg(not(unix))]
impl NativeSdkStateLease {
    #[cfg(test)]
    pub(crate) fn publish_session(&self, _: &str) -> Result<(), DataError> {
        Err(invalid(
            "native SDK state publication is unsupported on this platform",
        ))
    }

    pub(crate) fn validate(&self) -> Result<(), DataError> {
        Err(invalid(
            "native SDK state acquisition is unsupported on this platform",
        ))
    }
}

#[cfg(test)]
#[path = "native_sdk_state_test.rs"]
mod tests;

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
#[path = "native_sdk_writer_release_test.rs"]
mod writer_release_tests;

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
#[path = "native_sdk_run_state_test.rs"]
mod run_state_tests;
