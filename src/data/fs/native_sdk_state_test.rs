//! Issue 228 acquisition contract; acknowledged records here are prior-observer fixtures only.

#![cfg(unix)]

use super::*;
use crate::data::config::env::{EnvSnapshot, AWMAN_CONFIG_HOME};
use crate::data::fs::data_paths::DataPaths;
use std::error::Error;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{symlink, DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

struct Fixture {
    _home_artifact: tempfile::TempDir,
    base: PathBuf,
    project: PathBuf,
    operator_home: PathBuf,
    paths: DataPaths,
}

fn private_dir(path: &Path) -> io::Result<()> {
    DirBuilder::new().mode(0o700).create(path)
}

fn private_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?
        .write_all(bytes)
}

fn identity(path: &Path) -> io::Result<(u64, u64, u32, u32)> {
    let metadata = fs::symlink_metadata(path)?;
    Ok((
        metadata.dev(),
        metadata.ino(),
        metadata.uid(),
        metadata.mode() & 0o7777,
    ))
}

impl Fixture {
    fn new() -> TestResult<Self> {
        let home =
            dirs::home_dir().ok_or_else(|| io::Error::other("test requires an existing HOME"))?;
        let artifact = tempfile::Builder::new()
            .prefix("awman-native-state-contract-")
            .tempdir_in(home)?;
        let base = artifact.path().to_path_buf();
        let config_home = base.join("private-aw-home");
        let project = base.join("project");
        let operator_home = base.join("operator-home");
        private_dir(&config_home)?;
        private_dir(&project)?;
        private_dir(&operator_home)?;
        private_file(
            &operator_home.join("settings-sentinel"),
            b"operator settings unchanged",
        )?;
        let env = EnvSnapshot::with_overrides([(
            AWMAN_CONFIG_HOME,
            config_home.as_os_str().to_string_lossy(),
        )]);
        let paths = DataPaths::from_env(&env)?;
        private_dir(paths.root())?;
        Ok(Self {
            _home_artifact: artifact,
            base,
            project,
            operator_home,
            paths,
        })
    }

    fn key(&self, provider: NativeSdkProvider, model: &str) -> TestResult<NativeSdkStateKey> {
        Ok(NativeSdkStateKey::new(&self.project, provider, model)?)
    }

    fn state(&self) -> TestResult<NativeSdkState> {
        Ok(NativeSdkState::new(&self.paths)?)
    }
}

fn write_prior_observer_association(
    state: &NativeSdkState,
    key: &NativeSdkStateKey,
    lease: &NativeSdkStateLease,
    session_id: &str,
) -> TestResult<PathBuf> {
    let record = AssociationRecord {
        schema_version: SCHEMA_VERSION,
        session_id: session_id.to_owned(),
        owner: lease.owner_record.clone(),
    };
    let path = state.association_path(key, session_id)?;
    private_file(&path, &serde_json::to_vec(&record)?)?;
    Ok(path)
}

#[test]
fn constructor_requires_existing_safe_root_without_creating_ancestors() -> TestResult {
    let fixture = Fixture::new()?;
    let missing = fixture.base.join("missing-parent").join("data");
    assert!(NativeSdkState::new(&DataPaths::at_root(&missing)).is_err());
    assert!(!fixture.base.join("missing-parent").exists());
    let link = fixture.base.join("linked-data");
    symlink(fixture.paths.root(), &link)?;
    assert!(NativeSdkState::new(&DataPaths::at_root(&link)).is_err());
    fs::set_permissions(fixture.paths.root(), fs::Permissions::from_mode(0o777))?;
    assert!(fixture.state().is_err());
    Ok(())
}

#[test]
fn first_turns_create_distinct_private_owners_without_authorizing_resume() -> TestResult {
    for provider in [
        NativeSdkProvider::Claude,
        NativeSdkProvider::Codex,
        NativeSdkProvider::Agy,
    ] {
        let fixture = Fixture::new()?;
        let state = fixture.state()?;
        let key = fixture.key(provider, "explicit-model")?;
        let first = state.acquire_first(&key)?;
        let second = state.acquire_first(&key)?;
        assert_ne!(first.state_path(), second.state_path());
        assert_ne!(
            identity(first.state_path())?,
            identity(second.state_path())?
        );
        assert_ne!(
            first.owner_record.generation,
            second.owner_record.generation
        );
        for lease in [&first, &second] {
            assert!(lease.state_path().starts_with(fixture.paths.root()));
            assert!(!lease.state_path().starts_with(&fixture.project));
            assert_eq!(identity(lease.state_path())?.3, 0o700);
            lease.validate()?;
        }
        let retained = first.state_path().to_path_buf();
        private_file(&retained.join("copied-foreign-session"), b"foreign-session")?;
        drop(first);
        drop(second);
        assert!(retained.is_dir());
        assert!(state.acquire_resume(&key, "foreign-session").is_err());
        assert!(state.acquire_resume(&key, "missing-session").is_err());
    }
    Ok(())
}

#[test]
fn exact_prior_ack_resumes_same_identity_and_bytes_across_owner_instances() -> TestResult {
    for provider in [
        NativeSdkProvider::Claude,
        NativeSdkProvider::Codex,
        NativeSdkProvider::Agy,
    ] {
        let fixture = Fixture::new()?;
        let state = fixture.state()?;
        let key = fixture.key(provider, "explicit-model")?;
        let lease = state.acquire_first(&key)?;
        let path = lease.state_path().to_path_buf();
        let before = identity(&path)?;
        let generation = lease.owner_record.generation;
        private_file(&path.join("sdk-writable-sentinel"), b"private SDK mutation")?;
        write_prior_observer_association(&state, &key, &lease, "exact/session/../id")?;
        drop(lease);
        drop(state);
        let later_state = fixture.state()?;
        let resumed = later_state.acquire_resume(&key, "exact/session/../id")?;
        assert_eq!(resumed.state_path(), path);
        assert_eq!(identity(resumed.state_path())?, before);
        assert_eq!(resumed.owner_record.generation, generation);
        assert_eq!(
            fs::read(resumed.state_path().join("sdk-writable-sentinel"))?,
            b"private SDK mutation"
        );
        resumed.validate()?;
        assert!(later_state
            .acquire_resume(&key, "exact/session/../ID")
            .is_err());
    }
    Ok(())
}

#[test]
fn resume_rejects_wrong_provider_exact_model_and_replaced_project() -> TestResult {
    let fixture = Fixture::new()?;
    let state = fixture.state()?;
    let key = fixture.key(NativeSdkProvider::Codex, "explicit-model")?;
    let lease = state.acquire_first(&key)?;
    write_prior_observer_association(&state, &key, &lease, "ack-session")?;
    drop(lease);
    for provider in [NativeSdkProvider::Claude, NativeSdkProvider::Agy] {
        assert!(state
            .acquire_resume(&fixture.key(provider, "explicit-model")?, "ack-session")
            .is_err());
    }
    for model in ["other-model", "explicit-model ", "Explicit-model"] {
        assert!(state
            .acquire_resume(
                &fixture.key(NativeSdkProvider::Codex, model)?,
                "ack-session"
            )
            .is_err());
    }
    fs::rename(&fixture.project, fixture.base.join("original-project"))?;
    private_dir(&fixture.project)?;
    assert!(state.acquire_resume(&key, "ack-session").is_err());
    let rebound_key = fixture.key(NativeSdkProvider::Codex, "explicit-model")?;
    assert!(state.acquire_resume(&rebound_key, "ack-session").is_err());
    Ok(())
}

#[test]
fn invalid_session_ids_and_blank_models_fail_closed() -> TestResult {
    let fixture = Fixture::new()?;
    let state = fixture.state()?;
    let key = fixture.key(NativeSdkProvider::Claude, "explicit-model")?;
    for invalid in [
        String::new(),
        " ".into(),
        " leading".into(),
        "trailing ".into(),
        "id\nnext".into(),
        "id\u{0085}next".into(),
        "x".repeat(MAX_SESSION_ID_BYTES + 1),
        "é".repeat(MAX_SESSION_ID_BYTES / 2 + 1),
    ] {
        assert!(state.acquire_resume(&key, &invalid).is_err());
    }
    for blank in ["", " \t\n"] {
        assert!(
            NativeSdkStateKey::new(&fixture.project, NativeSdkProvider::Claude, blank).is_err()
        );
    }
    let maximum = "x".repeat(MAX_SESSION_ID_BYTES);
    let lease = state.acquire_first(&key)?;
    write_prior_observer_association(&state, &key, &lease, &maximum)?;
    drop(lease);
    state.acquire_resume(&key, &maximum)?.validate()?;
    Ok(())
}

#[test]
fn resume_rejects_corrupted_identity_and_association_records() -> TestResult {
    for corruption in 0..31 {
        let fixture = Fixture::new()?;
        let state = fixture.state()?;
        let key = fixture.key(NativeSdkProvider::Codex, "explicit-model")?;
        let lease = state.acquire_first(&key)?;
        let path = write_prior_observer_association(&state, &key, &lease, "ack-session")?;
        let mut record = AssociationRecord {
            schema_version: SCHEMA_VERSION,
            session_id: "ack-session".into(),
            owner: lease.owner_record.clone(),
        };
        drop(lease);
        match corruption {
            0 => record.schema_version = SCHEMA_VERSION + 1,
            1 => record.session_id = "different-session".into(),
            2 => record.owner.schema_version = SCHEMA_VERSION + 1,
            3 => record.owner.generation = uuid::Uuid::new_v4(),
            4 => record.owner.key.model = "different-model".into(),
            5 => record.owner.key.provider = NativeSdkProvider::Claude,
            6 => record.owner.key.project = fixture.operator_home.clone(),
            other => {
                let identity = match (other - 7) / 4 {
                    0 => &mut record.owner.key.project_identity,
                    1 => &mut record.owner.data_root,
                    2 => &mut record.owner.namespace,
                    3 => &mut record.owner.owner_dir,
                    4 => &mut record.owner.state_dir,
                    5 => &mut record.owner.writer_lock,
                    _ => unreachable!(),
                };
                match (other - 7) % 4 {
                    0 => identity.device ^= 1,
                    1 => identity.inode ^= 1,
                    2 => identity.owner_uid ^= 1,
                    3 => identity.mode ^= 1,
                    _ => unreachable!(),
                }
            }
        }
        fs::write(path, serde_json::to_vec(&record)?)?;
        assert!(
            state.acquire_resume(&key, "ack-session").is_err(),
            "accepted corruption {corruption}"
        );
    }
    Ok(())
}

#[test]
fn resume_rejects_unsafe_missing_and_oversized_index_records() -> TestResult {
    for corruption in 0..6 {
        let fixture = Fixture::new()?;
        let state = fixture.state()?;
        let key = fixture.key(NativeSdkProvider::Agy, "explicit-model")?;
        let lease = state.acquire_first(&key)?;
        let path = write_prior_observer_association(&state, &key, &lease, "ack-session")?;
        drop(lease);
        match corruption {
            0 => fs::remove_file(&path)?,
            1 => fs::write(&path, b"{invalid")?,
            2 => fs::write(&path, vec![b' '; MAX_RECORD_BYTES as usize + 1])?,
            3 => fs::set_permissions(&path, fs::Permissions::from_mode(0o644))?,
            4 => {
                let saved = fixture.base.join("original-index");
                fs::rename(&path, &saved)?;
                symlink(&saved, &path)?;
            }
            5 => fs::hard_link(&path, fixture.base.join("index-hardlink"))?,
            _ => unreachable!(),
        }
        assert!(
            state.acquire_resume(&key, "ack-session").is_err(),
            "accepted unsafe index {corruption}"
        );
    }
    Ok(())
}

#[test]
fn unsafe_owner_files_and_directory_modes_fail_closed() -> TestResult {
    for target in 0..5 {
        let fixture = Fixture::new()?;
        let state = fixture.state()?;
        let key = fixture.key(NativeSdkProvider::Claude, "explicit-model")?;
        let lease = state.acquire_first(&key)?;
        write_prior_observer_association(&state, &key, &lease, "ack-session")?;
        let path = match target {
            0 => state.namespace_path(&key),
            1 => lease.owner_path.clone(),
            2 => lease.state_path().to_path_buf(),
            3 => lease.owner_path.join("owner.json"),
            4 => lease.owner_path.join("writer.lock"),
            _ => unreachable!(),
        };
        fs::set_permissions(
            &path,
            fs::Permissions::from_mode(if target < 3 { 0o755 } else { 0o644 }),
        )?;
        assert!(
            lease.validate().is_err(),
            "accepted unsafe held path {target}"
        );
        drop(lease);
        assert!(
            state.acquire_resume(&key, "ack-session").is_err(),
            "accepted unsafe resume path {target}"
        );
    }
    for filename in ["owner.json", "writer.lock"] {
        let fixture = Fixture::new()?;
        let state = fixture.state()?;
        let key = fixture.key(NativeSdkProvider::Claude, "explicit-model")?;
        let lease = state.acquire_first(&key)?;
        write_prior_observer_association(&state, &key, &lease, "ack-session")?;
        let path = lease.owner_path.join(filename);
        drop(lease);
        fs::hard_link(&path, fixture.base.join("foreign-hardlink"))?;
        assert!(
            state.acquire_resume(&key, "ack-session").is_err(),
            "accepted hardlinked {filename}"
        );
    }
    Ok(())
}

#[test]
fn exchanged_or_symlinked_owned_paths_invalidate_held_and_future_leases() -> TestResult {
    for target in 0..5 {
        for replacement_is_symlink in [false, true] {
            let fixture = Fixture::new()?;
            let state = fixture.state()?;
            let key = fixture.key(NativeSdkProvider::Codex, "explicit-model")?;
            let lease = state.acquire_first(&key)?;
            write_prior_observer_association(&state, &key, &lease, "ack-session")?;
            let path = match target {
                0 => fixture.paths.root().to_path_buf(),
                1 => state.namespace_path(&key),
                2 => lease.owner_path.clone(),
                3 => lease.state_path().to_path_buf(),
                4 => lease.owner_path.join("writer.lock"),
                _ => unreachable!(),
            };
            let saved = fixture.base.join("original-owned-path");
            fs::rename(&path, &saved)?;
            if replacement_is_symlink {
                symlink(&saved, &path)?;
            } else if target == 4 {
                private_file(&path, b"")?;
            } else {
                private_dir(&path)?;
            }
            assert!(
                lease.validate().is_err(),
                "accepted held path exchange {target}"
            );
            assert!(
                state.acquire_resume(&key, "ack-session").is_err(),
                "accepted resume path exchange {target}"
            );
        }
    }
    Ok(())
}

#[test]
fn acquisition_never_mutates_operator_settings_or_other_owner_state() -> TestResult {
    let fixture = Fixture::new()?;
    let operator_file = fixture.operator_home.join("settings-sentinel");
    let before = identity(&operator_file)?;
    let state = fixture.state()?;
    let key = fixture.key(NativeSdkProvider::Codex, "explicit-model")?;
    let first = state.acquire_first(&key)?;
    let original = first.state_path().to_path_buf();
    private_file(&original.join("first-conversation"), b"first only")?;
    write_prior_observer_association(&state, &key, &first, "ack-session")?;
    drop(first);
    let other = state.acquire_first(&key)?;
    assert!(!other.state_path().join("first-conversation").exists());
    drop(other);
    let resumed = state.acquire_resume(&key, "ack-session")?;
    private_file(
        &resumed.state_path().join("later-conversation"),
        b"private later state",
    )?;
    assert_eq!(
        fs::read(original.join("first-conversation"))?,
        b"first only"
    );
    assert_eq!(identity(&operator_file)?, before);
    assert_eq!(fs::read(&operator_file)?, b"operator settings unchanged");
    assert_eq!(fs::read_dir(&fixture.operator_home)?.count(), 1);
    Ok(())
}

#[test]
fn concurrent_resume_child() -> TestResult {
    let Some(base) = std::env::var_os("AWMAN_TEST_NATIVE_LOCK_BASE") else {
        return Ok(());
    };
    let base = PathBuf::from(base);
    let paths = DataPaths::at_root(base.join("private-aw-home/data"));
    let state = NativeSdkState::new(&paths)?;
    let key = NativeSdkStateKey::new(
        &base.join("project"),
        NativeSdkProvider::Codex,
        "explicit-model",
    )?;
    assert!(state.acquire_resume(&key, "concurrent-session").is_err());
    private_file(&base.join("child-observed-lock-rejection"), b"rejected")?;
    Ok(())
}

#[test]
fn exclusive_writer_lock_rejects_another_process_and_releases_without_deletion() -> TestResult {
    let fixture = Fixture::new()?;
    let state = fixture.state()?;
    let key = fixture.key(NativeSdkProvider::Codex, "explicit-model")?;
    let lease = state.acquire_first(&key)?;
    let retained = lease.state_path().to_path_buf();
    write_prior_observer_association(&state, &key, &lease, "concurrent-session")?;
    assert!(state.acquire_resume(&key, "concurrent-session").is_err());
    let mut child = Command::new(std::env::current_exe()?)
        .arg("--exact")
        .arg("data::fs::native_sdk_state::tests::concurrent_resume_child")
        .arg("--test-threads=1")
        .env("AWMAN_TEST_NATIVE_LOCK_BASE", &fixture.base)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill()?;
            child.wait()?;
            return Err(io::Error::other(
                "concurrent acquisition child did not finish within 10 seconds",
            )
            .into());
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(status.success());
    assert_eq!(
        fs::read(fixture.base.join("child-observed-lock-rejection"))?,
        b"rejected"
    );
    lease.validate()?;
    drop(lease);
    assert!(retained.is_dir());
    let resumed = state.acquire_resume(&key, "concurrent-session")?;
    assert_eq!(resumed.state_path(), retained);
    resumed.validate()?;
    Ok(())
}

#[test]
fn acquisition_rejects_data_root_equal_to_or_beneath_project() -> TestResult {
    for nested in [false, true] {
        let fixture = Fixture::new()?;
        let root = if nested {
            let root = fixture.project.join("private-aw-data");
            private_dir(&root)?;
            root
        } else {
            fixture.project.clone()
        };
        let state = NativeSdkState::new(&DataPaths::at_root(&root))?;
        let key = fixture.key(NativeSdkProvider::Codex, "explicit-model")?;
        let namespace = state.namespace_path(&key);
        assert!(
            state.acquire_first(&key).is_err(),
            "accepted SDK state within the project: nested={nested}"
        );
        assert!(!namespace.exists());
    }
    Ok(())
}

#[test]
fn fresh_instance_rejects_replaced_store_with_original_namespace_retained() -> TestResult {
    let fixture = Fixture::new()?;
    let state = fixture.state()?;
    let key = fixture.key(NativeSdkProvider::Codex, "explicit-model")?;
    let lease = state.acquire_first(&key)?;
    let namespace = state.namespace_path(&key);
    let state_path = lease.state_path().to_path_buf();
    let owner_record = lease.owner_path.join("owner.json");
    let writer_lock = lease.owner_path.join("writer.lock");
    let association =
        write_prior_observer_association(&state, &key, &lease, "store-exchange-session")?;
    let sentinel = state_path.join("sdk-writable-sentinel");
    private_file(&sentinel, b"retained private SDK mutation")?;
    let retained_paths = [
        &namespace,
        &state_path,
        &owner_record,
        &writer_lock,
        &association,
    ];
    let retained_identities = retained_paths
        .iter()
        .map(|path| identity(path))
        .collect::<io::Result<Vec<_>>>()?;
    let owner_bytes = fs::read(&owner_record)?;
    let association_bytes = fs::read(&association)?;
    let root_identity = identity(fixture.paths.root())?;
    let store = fixture.paths.root().join("native-sdk-state");
    let store_identity = identity(&store)?;
    drop(lease);
    drop(state);
    let saved_store = fixture.base.join("original-native-sdk-state");
    fs::rename(&store, &saved_store)?;
    private_dir(&store)?;
    let namespace_name = namespace
        .file_name()
        .ok_or_else(|| io::Error::other("namespace requires a final path component"))?;
    fs::rename(saved_store.join(namespace_name), &namespace)?;
    assert_eq!(identity(fixture.paths.root())?, root_identity);
    assert_ne!(identity(&store)?, store_identity);
    for (path, before) in retained_paths.iter().zip(&retained_identities) {
        assert_eq!(identity(path)?, *before);
    }
    assert_eq!(fs::read(&owner_record)?, owner_bytes);
    assert_eq!(fs::read(&association)?, association_bytes);
    let later_state = fixture.state()?;
    assert!(
        later_state
            .acquire_resume(&key, "store-exchange-session")
            .is_err(),
        "fresh owner accepted an exchanged native SDK state store"
    );
    for (path, before) in retained_paths.iter().zip(&retained_identities) {
        assert_eq!(identity(path)?, *before);
    }
    assert_eq!(fs::read(&owner_record)?, owner_bytes);
    assert_eq!(fs::read(&association)?, association_bytes);
    assert_eq!(fs::read(&sentinel)?, b"retained private SDK mutation");
    Ok(())
}
