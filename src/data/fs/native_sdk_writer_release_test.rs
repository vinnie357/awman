//! Existing SDK leases release their writer authority before unrelated children exec.

use super::*;
use awman_test_spawn_barrier::SpawnBarrier;
use std::error::Error;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;
type Identity = (u64, u64, u32, u32);

fn identity(path: &Path) -> io::Result<Identity> {
    let metadata = fs::symlink_metadata(path)?;
    Ok((
        metadata.dev(),
        metadata.ino(),
        metadata.uid(),
        metadata.mode() & 0o7777,
    ))
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

struct RetainedState {
    state_path: PathBuf,
    identities: Vec<(PathBuf, Identity)>,
    files: Vec<(PathBuf, Vec<u8>)>,
}

impl RetainedState {
    fn capture(
        state: &NativeSdkState,
        key: &NativeSdkStateKey,
        lease: &NativeSdkStateLease,
        session: &str,
    ) -> TestResult<Self> {
        let state_path = lease.state_path().to_path_buf();
        let owner = state_path
            .parent()
            .ok_or_else(|| io::Error::other("state has no owner directory"))?;
        let paths = [
            owner.to_path_buf(),
            state_path.clone(),
            owner.join("writer.lock"),
            owner.join("owner.json"),
            state.association_path(key, session)?,
            state_path.join("sdk-mutation"),
        ];
        let identities = paths
            .iter()
            .map(|path| Ok((path.clone(), identity(path)?)))
            .collect::<io::Result<_>>()?;
        let files = paths[2..]
            .iter()
            .map(|path| Ok((path.clone(), fs::read(path)?)))
            .collect::<io::Result<_>>()?;
        Ok(Self {
            state_path,
            identities,
            files,
        })
    }

    fn assert_preserved(&self, lease: &NativeSdkStateLease) -> TestResult {
        assert_eq!(lease.state_path(), self.state_path);
        for (path, expected) in &self.identities {
            assert_eq!(
                &identity(path)?,
                expected,
                "identity changed: {}",
                path.display()
            );
        }
        for (path, expected) in &self.files {
            assert_eq!(
                &fs::read(path)?,
                expected,
                "bytes changed: {}",
                path.display()
            );
        }
        lease.validate()?;
        Ok(())
    }
}

fn assert_active_writer(
    state: &NativeSdkState,
    key: &NativeSdkStateKey,
    session: &str,
) -> TestResult {
    match state.acquire_resume(key, session) {
        Ok(_) => {
            return Err(io::Error::other(
                "resume accepted while an SDK lease still owned the writer",
            )
            .into())
        }
        Err(error) => assert_eq!(
            error.to_string(),
            "native SDK state: native state already has an active writer"
        ),
    }
    Ok(())
}

#[test]
fn dropped_owner_releases_exact_sdk_state_before_unrelated_child_exec() -> TestResult {
    let home =
        dirs::home_dir().ok_or_else(|| io::Error::other("test requires an existing HOME"))?;
    let artifact = tempfile::Builder::new()
        .prefix("awman-sdk-writer-release-")
        .tempdir_in(home)?;
    let project = artifact.path().join("project");
    let paths = DataPaths::at_root(artifact.path().join("data"));
    private_dir(&project)?;
    private_dir(paths.root())?;
    let key = NativeSdkStateKey::new(&project, NativeSdkProvider::Codex, "explicit-model")?;
    let state = NativeSdkState::new(&paths)?;
    let lease = state.acquire_first(&key)?;
    let session = "exact/session/../writer-release";
    private_file(
        &lease.state_path().join("sdk-mutation"),
        b"private SDK mutation survives owner release",
    )?;
    lease.publish_session(session)?;
    let retained = RetainedState::capture(&state, &key, &lease, session)?;
    let mut child = SpawnBarrier::start()?;
    assert_active_writer(&state, &key, session)?;
    drop(lease);
    drop(state);
    let later_state = NativeSdkState::new(&paths)?;
    let resumed = match later_state.acquire_resume(&key, session) {
        Ok(resumed) => resumed,
        Err(error) => {
            child.release_and_wait()?;
            return Err(io::Error::other(format!("owner drop must permit immediate exact SDK resume while unrelated child is paused: {error}")).into());
        }
    };
    retained.assert_preserved(&resumed)?;
    assert_active_writer(&later_state, &key, session)?;
    child.release_and_wait()?;
    retained.assert_preserved(&resumed)?;
    assert_active_writer(&later_state, &key, session)?;
    drop(resumed);
    let final_resume = later_state.acquire_resume(&key, session)?;
    retained.assert_preserved(&final_resume)?;
    Ok(())
}
