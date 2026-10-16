//! Accepted ADR 0001 storage ordering only; completion calls do not prove execution retirement.

use super::*;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::error::Error;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{symlink, DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;
type Identity = (u64, u64, u32, u32, u64);
const MODEL: &str = "explicit-model";
const SESSION: &str = "exact/session/../run-state";
const MUTATION_A: &[u8] = b"SDK mutation from first storage round";
const MUTATION_B: &[u8] = b"SDK mutation from resumed storage round";
const ACTIVE: &str = "active-run.json";
const COMPLETED: &str = "completed-run.json";

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

fn overwrite(path: &Path, bytes: &[u8]) -> io::Result<()> {
    OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)?
        .write_all(bytes)
}

fn identity(path: &Path) -> io::Result<Identity> {
    let metadata = fs::symlink_metadata(path)?;
    Ok((
        metadata.dev(),
        metadata.ino(),
        metadata.uid(),
        metadata.mode() & 0o7777,
        metadata.nlink(),
    ))
}

fn owner_path(state_path: &Path) -> TestResult<PathBuf> {
    Ok(state_path
        .parent()
        .ok_or_else(|| io::Error::other("state has no owner"))?
        .to_path_buf())
}

fn run_path(state_path: &Path, name: &str) -> TestResult<PathBuf> {
    Ok(owner_path(state_path)?.join(name))
}

fn read_json(path: &Path) -> TestResult<Value> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

fn require_completed(outcome: NativeSdkRunCompletion) -> TestResult {
    match outcome {
        NativeSdkRunCompletion::Completed => Ok(()),
        NativeSdkRunCompletion::CompletedPublicationUncertain(error) => Err(io::Error::other(
            format!("expected durable completion, got uncertainty: {error}"),
        )
        .into()),
    }
}

fn require_uncertain(outcome: NativeSdkRunCompletion) -> TestResult {
    match outcome {
        NativeSdkRunCompletion::CompletedPublicationUncertain(_) => Ok(()),
        NativeSdkRunCompletion::Completed => {
            Err(io::Error::other("visible-completion fault was reported as durable success").into())
        }
    }
}

fn require_not_completed(outcome: Result<NativeSdkRunCompletion, DataError>) -> TestResult {
    match outcome {
        Err(_) | Ok(NativeSdkRunCompletion::CompletedPublicationUncertain(_)) => Ok(()),
        Ok(NativeSdkRunCompletion::Completed) => {
            Err(io::Error::other("failed reconciliation reported Completed").into())
        }
    }
}

struct Fixture {
    _artifact: tempfile::TempDir,
    base: PathBuf,
    project: PathBuf,
    paths: DataPaths,
}

impl Fixture {
    fn new() -> TestResult<Self> {
        let home =
            dirs::home_dir().ok_or_else(|| io::Error::other("test requires an existing HOME"))?;
        let artifact = tempfile::Builder::new()
            .prefix("awman-native-run-state-")
            .tempdir_in(home)?;
        let base = artifact.path().to_path_buf();
        let project = base.join("project");
        let paths = DataPaths::at_root(base.join("data"));
        private_dir(&project)?;
        private_dir(paths.root())?;
        Ok(Self {
            _artifact: artifact,
            base,
            project,
            paths,
        })
    }

    fn state(&self) -> TestResult<NativeSdkState> {
        Ok(NativeSdkState::new(&self.paths)?)
    }

    fn key(&self, provider: NativeSdkProvider) -> TestResult<NativeSdkStateKey> {
        Ok(NativeSdkStateKey::new(&self.project, provider, MODEL)?)
    }

    fn snapshot(&self) -> TestResult<TreeSnapshot> {
        tree_snapshot(&self.base)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Entry {
    Directory,
    File(Vec<u8>),
    Symlink(PathBuf),
}

type TreeSnapshot = BTreeMap<PathBuf, (Identity, Entry)>;

fn tree_snapshot(base: &Path) -> TestResult<TreeSnapshot> {
    fn visit(base: &Path, path: &Path, snapshot: &mut TreeSnapshot) -> TestResult {
        let metadata = fs::symlink_metadata(path)?;
        let entry = if metadata.file_type().is_symlink() {
            Entry::Symlink(fs::read_link(path)?)
        } else if metadata.is_dir() {
            Entry::Directory
        } else {
            Entry::File(fs::read(path)?)
        };
        snapshot.insert(
            path.strip_prefix(base)?.to_path_buf(),
            (identity(path)?, entry),
        );
        if metadata.is_dir() {
            for entry in fs::read_dir(path)? {
                visit(base, &entry?.path(), snapshot)?;
            }
        }
        Ok(())
    }
    let mut snapshot = BTreeMap::new();
    visit(base, base, &mut snapshot)?;
    Ok(snapshot)
}

struct StableState {
    state_path: PathBuf,
    identities: Vec<(PathBuf, Identity)>,
    owner_bytes: Vec<u8>,
    lock_bytes: Vec<u8>,
}

impl StableState {
    fn capture(fixture: &Fixture, path: &Path) -> TestResult<Self> {
        let owner = owner_path(path)?;
        let owners = owner
            .parent()
            .ok_or_else(|| io::Error::other("owner has no namespace"))?;
        let namespace = owners
            .parent()
            .ok_or_else(|| io::Error::other("owners has no namespace"))?;
        let paths = [
            fixture.project.clone(),
            fixture.paths.root().to_path_buf(),
            fixture.paths.root().join("native-sdk-state"),
            namespace.to_path_buf(),
            owners.to_path_buf(),
            namespace.join("sessions"),
            owner.clone(),
            path.to_path_buf(),
            owner.join("owner.json"),
            owner.join("writer.lock"),
        ];
        let identities = paths
            .into_iter()
            .map(|path| Ok((path.clone(), identity(&path)?)))
            .collect::<io::Result<_>>()?;
        Ok(Self {
            state_path: path.to_path_buf(),
            identities,
            owner_bytes: fs::read(owner.join("owner.json"))?,
            lock_bytes: fs::read(owner.join("writer.lock"))?,
        })
    }

    fn assert_preserved(&self, path: &Path, mutation: &[u8]) -> TestResult {
        assert_eq!(path, self.state_path);
        for (path, expected) in &self.identities {
            assert_eq!(
                &identity(path)?,
                expected,
                "physical owner changed: {}",
                path.display()
            );
        }
        let owner = owner_path(path)?;
        assert_eq!(fs::read(owner.join("owner.json"))?, self.owner_bytes);
        assert_eq!(fs::read(owner.join("writer.lock"))?, self.lock_bytes);
        assert_eq!(fs::read(path.join("sdk-mutation"))?, mutation);
        Ok(())
    }
}

fn assert_record_shape(record: &Value, completion: bool) -> TestResult {
    let object = record
        .as_object()
        .ok_or_else(|| io::Error::other("run record is not an object"))?;
    let mut expected = vec!["owner", "requested_session", "run_id", "schema_version"];
    if completion {
        expected.push("session_id");
    }
    expected.sort_unstable();
    let mut actual = object.keys().map(String::as_str).collect::<Vec<_>>();
    actual.sort_unstable();
    assert_eq!(actual, expected);
    assert!(object.contains_key("requested_session"));
    assert_eq!(object.get("schema_version"), Some(&json!(1)));
    let run = object
        .get("run_id")
        .and_then(Value::as_str)
        .ok_or_else(|| io::Error::other("missing run UUID"))?;
    Uuid::parse_str(run)?;
    Ok(())
}

fn activation(path: &Path, requested: Option<&str>) -> TestResult<Value> {
    let value = read_json(&run_path(path, ACTIVE)?)?;
    assert_record_shape(&value, false)?;
    assert_eq!(
        value.get("owner"),
        Some(&read_json(&owner_path(path)?.join("owner.json"))?)
    );
    assert_eq!(
        value.get("requested_session"),
        Some(&requested.map_or(Value::Null, |id| json!(id)))
    );
    assert_eq!(identity(&run_path(path, ACTIVE)?)?.3, 0o600);
    assert_eq!(identity(&run_path(path, ACTIVE)?)?.4, 1);
    Ok(value)
}

fn completion(path: &Path, expected_active: &Value) -> TestResult<Value> {
    let value = read_json(&run_path(path, COMPLETED)?)?;
    assert_record_shape(&value, true)?;
    for field in ["schema_version", "run_id", "owner", "requested_session"] {
        assert_eq!(
            value.get(field),
            expected_active.get(field),
            "completion differs at {field}"
        );
    }
    assert_eq!(value.get("session_id"), Some(&json!(SESSION)));
    assert_eq!(identity(&run_path(path, COMPLETED)?)?.3, 0o600);
    assert_eq!(identity(&run_path(path, COMPLETED)?)?.4, 1);
    Ok(value)
}

fn assert_legacy_writer_held(state: &NativeSdkState, key: &NativeSdkStateKey) -> TestResult {
    match state.acquire_resume(key, SESSION) {
        Ok(_) => {
            return Err(
                io::Error::other("run wrapper released its underlying writer authority").into(),
            )
        }
        Err(error) => assert_eq!(
            error.to_string(),
            "native SDK state: native state already has an active writer"
        ),
    }
    Ok(())
}

struct Established {
    state: NativeSdkState,
    key: NativeSdkStateKey,
    stable: StableState,
    active_a: Value,
    completion_a: Vec<u8>,
    association: PathBuf,
}

fn establish(fixture: &Fixture) -> TestResult<Established> {
    let state = fixture.state()?;
    let key = fixture.key(NativeSdkProvider::Codex)?;
    let mut run = state.acquire_first_run(&key)?;
    private_file(&run.state_path().join("sdk-mutation"), MUTATION_A)?;
    let stable = StableState::capture(fixture, run.state_path())?;
    let active_a = activation(run.state_path(), None)?;
    require_completed(run.complete_run(SESSION)?)?;
    completion(run.state_path(), &active_a)?;
    let completion_a = fs::read(run_path(run.state_path(), COMPLETED)?)?;
    let association = state.association_path(&key, SESSION)?;
    drop(run);
    Ok(Established {
        state,
        key,
        stable,
        active_a,
        completion_a,
        association,
    })
}

#[test]
fn strict_resume_rejects_absent_legacy_only_and_invalid_session_without_writes() -> TestResult {
    let fixture = Fixture::new()?;
    let state = fixture.state()?;
    let key = fixture.key(NativeSdkProvider::Codex)?;
    let empty = fixture.snapshot()?;
    assert!(state.acquire_resume_run(&key, SESSION).is_err());
    assert_eq!(fixture.snapshot()?, empty);
    let legacy = state.acquire_first(&key)?;
    private_file(&legacy.state_path().join("sdk-mutation"), MUTATION_A)?;
    legacy.publish_session(SESSION)?;
    let retained = legacy.state_path().to_path_buf();
    drop(legacy);
    let before = fixture.snapshot()?;
    assert!(state.acquire_resume_run(&key, SESSION).is_err());
    for invalid in [
        "".to_owned(),
        " leading".to_owned(),
        "trailing ".to_owned(),
        "bad\nrequest".to_owned(),
        "x".repeat(1025),
    ] {
        assert!(state.acquire_resume_run(&key, &invalid).is_err());
    }
    assert!(!run_path(&retained, ACTIVE)?.exists());
    assert!(!run_path(&retained, COMPLETED)?.exists());
    assert_eq!(fixture.snapshot()?, before);
    let compatibility = state.acquire_resume(&key, SESSION)?;
    assert_eq!(compatibility.state_path(), retained);
    Ok(())
}

#[test]
fn two_rounds_preserve_owner_and_advance_completion_even_on_association_fastpath() -> TestResult {
    for provider in [
        NativeSdkProvider::Claude,
        NativeSdkProvider::Codex,
        NativeSdkProvider::Agy,
    ] {
        let fixture = Fixture::new()?;
        let state = fixture.state()?;
        let key = fixture.key(provider)?;
        let mut first = state.acquire_first_run(&key)?;
        first.validate_request(provider, MODEL, None)?;
        assert!(first
            .validate_request(provider, "wrong-model", None)
            .is_err());
        assert!(first
            .validate_request(provider, MODEL, Some(SESSION))
            .is_err());
        private_file(&first.state_path().join("sdk-mutation"), MUTATION_A)?;
        let stable = StableState::capture(&fixture, first.state_path())?;
        let active_a = activation(first.state_path(), None)?;
        assert!(!run_path(first.state_path(), COMPLETED)?.exists());
        assert!(!state.association_path(&key, SESSION)?.exists());
        let active_a_bytes = fs::read(run_path(first.state_path(), ACTIVE)?)?;
        require_completed(first.complete_run(SESSION)?)?;
        let completion_a = completion(first.state_path(), &active_a)?;
        let clean_completed = fixture.snapshot()?;
        assert!(first.complete_run(SESSION).is_err());
        assert_eq!(fixture.snapshot()?, clean_completed);
        assert!(first.reconcile_completed().is_err());
        assert_eq!(fixture.snapshot()?, clean_completed);
        assert_eq!(
            read_json(&state.association_path(&key, SESSION)?)?.get("owner"),
            active_a.get("owner")
        );
        stable.assert_preserved(first.state_path(), MUTATION_A)?;
        assert_eq!(
            fs::read(run_path(first.state_path(), ACTIVE)?)?,
            active_a_bytes
        );
        drop(first);
        drop(state);
        let later = fixture.state()?;
        let mut second = later.acquire_resume_run(&key, SESSION)?;
        second.validate_request(provider, MODEL, Some(SESSION))?;
        let active_b = activation(second.state_path(), Some(SESSION))?;
        assert_ne!(active_b.get("run_id"), active_a.get("run_id"));
        assert_eq!(
            read_json(&run_path(second.state_path(), COMPLETED)?)?,
            completion_a
        );
        stable.assert_preserved(second.state_path(), MUTATION_A)?;
        overwrite(&second.state_path().join("sdk-mutation"), MUTATION_B)?;
        let association_before = fs::read(later.association_path(&key, SESSION)?)?;
        require_completed(second.complete_run_with_fault(
            SESSION,
            Some(NativeSdkRunFsFault::AfterAssociationRename),
        )?)?;
        completion(second.state_path(), &active_b)?;
        assert_eq!(
            fs::read(later.association_path(&key, SESSION)?)?,
            association_before
        );
        assert_eq!(activation(second.state_path(), Some(SESSION))?, active_b);
        stable.assert_preserved(second.state_path(), MUTATION_B)?;
        drop(second);
        let third = later.acquire_resume_run(&key, SESSION)?;
        let active_c = activation(third.state_path(), Some(SESSION))?;
        assert_ne!(active_c.get("run_id"), active_b.get("run_id"));
        completion(third.state_path(), &active_b)?;
        stable.assert_preserved(third.state_path(), MUTATION_B)?;
        assert_legacy_writer_held(&later, &key)?;
    }
    Ok(())
}

#[test]
fn interrupted_resume_releases_os_lock_but_old_completion_cannot_authorize_next_run() -> TestResult
{
    let fixture = Fixture::new()?;
    let established = establish(&fixture)?;
    let run = established
        .state
        .acquire_resume_run(&established.key, SESSION)?;
    let active_b = activation(run.state_path(), Some(SESSION))?;
    assert_ne!(active_b.get("run_id"), established.active_a.get("run_id"));
    overwrite(&run.state_path().join("sdk-mutation"), MUTATION_B)?;
    drop(run);
    drop(established.state);
    let state = fixture.state()?;
    let before = fixture.snapshot()?;
    assert!(state.acquire_resume_run(&established.key, SESSION).is_err());
    assert_eq!(fixture.snapshot()?, before);
    assert_eq!(
        fs::read(run_path(&established.stable.state_path, COMPLETED)?)?,
        established.completion_a
    );
    established
        .stable
        .assert_preserved(&established.stable.state_path, MUTATION_B)?;
    let legacy = state.acquire_resume(&established.key, SESSION)?;
    assert_eq!(legacy.state_path(), established.stable.state_path);
    Ok(())
}

#[test]
fn activation_failure_after_real_rename_preserves_unmatched_b_and_prior_completion() -> TestResult {
    let fixture = Fixture::new()?;
    let established = establish(&fixture)?;
    let association_before = fs::read(&established.association)?;
    assert!(established
        .state
        .acquire_resume_run_with_fault(
            &established.key,
            SESSION,
            Some(NativeSdkRunFsFault::AfterActivationRename)
        )
        .is_err());
    assert_eq!(fs::read(&established.association)?, association_before);
    let active_b = activation(&established.stable.state_path, Some(SESSION))?;
    assert_ne!(active_b.get("run_id"), established.active_a.get("run_id"));
    assert_eq!(
        fs::read(run_path(&established.stable.state_path, COMPLETED)?)?,
        established.completion_a
    );
    established
        .stable
        .assert_preserved(&established.stable.state_path, MUTATION_A)?;
    let before = fixture.snapshot()?;
    assert!(established
        .state
        .acquire_resume_run(&established.key, SESSION)
        .is_err());
    assert_eq!(fixture.snapshot()?, before);
    let legacy = established
        .state
        .acquire_resume(&established.key, SESSION)?;
    assert_eq!(legacy.state_path(), established.stable.state_path);
    Ok(())
}

#[test]
fn first_activation_fault_reports_error_only_after_actual_active_record_exists() -> TestResult {
    let fixture = Fixture::new()?;
    let state = fixture.state()?;
    let key = fixture.key(NativeSdkProvider::Codex)?;
    assert!(state
        .acquire_first_run_with_fault(&key, Some(NativeSdkRunFsFault::AfterActivationRename))
        .is_err());
    let owners = state.namespace_path(&key).join("owners");
    let entries = fs::read_dir(&owners)?.collect::<io::Result<Vec<_>>>()?;
    assert_eq!(entries.len(), 1);
    let state_path = entries[0].path().join("state");
    activation(&state_path, None)?;
    assert!(!run_path(&state_path, COMPLETED)?.exists());
    assert!(!state.association_path(&key, SESSION)?.exists());
    let before = fixture.snapshot()?;
    assert!(state.acquire_resume_run(&key, SESSION).is_err());
    assert_eq!(fixture.snapshot()?, before);
    Ok(())
}

#[test]
fn invalid_or_changed_acknowledgement_cannot_write_association_or_completion() -> TestResult {
    let fixture = Fixture::new()?;
    let established = establish(&fixture)?;
    let mut run = established
        .state
        .acquire_resume_run(&established.key, SESSION)?;
    let before = fixture.snapshot()?;
    for invalid in [
        "".to_owned(),
        " invalid".to_owned(),
        "bad\nack".to_owned(),
        "x".repeat(1025),
        "other-valid-session".to_owned(),
    ] {
        assert!(run.complete_run(&invalid).is_err());
        assert_eq!(fixture.snapshot()?, before);
    }
    require_completed(run.complete_run(SESSION)?)?;
    established
        .stable
        .assert_preserved(run.state_path(), MUTATION_A)?;
    Ok(())
}

#[test]
fn association_rename_failure_is_ordinary_error_and_cannot_create_completion() -> TestResult {
    let fixture = Fixture::new()?;
    let state = fixture.state()?;
    let key = fixture.key(NativeSdkProvider::Codex)?;
    let mut run = state.acquire_first_run(&key)?;
    private_file(&run.state_path().join("sdk-mutation"), MUTATION_A)?;
    let stable = StableState::capture(&fixture, run.state_path())?;
    let active = fs::read(run_path(run.state_path(), ACTIVE)?)?;
    assert!(run
        .complete_run_with_fault(SESSION, Some(NativeSdkRunFsFault::AfterAssociationRename))
        .is_err());
    assert!(state.association_path(&key, SESSION)?.is_file());
    assert_eq!(fs::read(run_path(run.state_path(), ACTIVE)?)?, active);
    assert!(!run_path(run.state_path(), COMPLETED)?.exists());
    stable.assert_preserved(run.state_path(), MUTATION_A)?;
    assert_legacy_writer_held(&state, &key)?;
    drop(run);
    drop(state);
    let later = fixture.state()?;
    let before = fixture.snapshot()?;
    assert!(later.acquire_resume_run(&key, SESSION).is_err());
    assert_eq!(fixture.snapshot()?, before);
    stable.assert_preserved(&stable.state_path, MUTATION_A)?;
    Ok(())
}

#[test]
fn active_storage_retry_validates_visible_association_then_publishes_matching_completion(
) -> TestResult {
    let fixture = Fixture::new()?;
    let state = fixture.state()?;
    let key = fixture.key(NativeSdkProvider::Codex)?;
    let mut run = state.acquire_first_run(&key)?;
    private_file(&run.state_path().join("sdk-mutation"), MUTATION_A)?;
    let stable = StableState::capture(&fixture, run.state_path())?;
    let active = activation(run.state_path(), None)?;
    assert!(run
        .complete_run_with_fault(SESSION, Some(NativeSdkRunFsFault::AfterAssociationRename))
        .is_err());
    let association = fs::read(state.association_path(&key, SESSION)?)?;
    require_completed(
        run.complete_run_with_fault(SESSION, Some(NativeSdkRunFsFault::AfterAssociationRename))?,
    )?;
    assert_eq!(
        fs::read(state.association_path(&key, SESSION)?)?,
        association
    );
    assert_eq!(activation(run.state_path(), None)?, active);
    completion(run.state_path(), &active)?;
    stable.assert_preserved(run.state_path(), MUTATION_A)?;
    drop(run);
    let resumed = state.acquire_resume_run(&key, SESSION)?;
    stable.assert_preserved(resumed.state_path(), MUTATION_A)?;
    Ok(())
}

fn completion_fault(index: usize) -> NativeSdkRunFsFault {
    if index == 0 {
        NativeSdkRunFsFault::AfterCompletionRename
    } else {
        NativeSdkRunFsFault::AfterCompletionDirectorySync
    }
}

#[test]
fn visible_completion_faults_retain_writer_and_reconcile_without_new_activation_or_bytes(
) -> TestResult {
    for fault in 0..2 {
        let fixture = Fixture::new()?;
        let state = fixture.state()?;
        let key = fixture.key(NativeSdkProvider::Codex)?;
        let mut run = state.acquire_first_run(&key)?;
        private_file(&run.state_path().join("sdk-mutation"), MUTATION_A)?;
        let stable = StableState::capture(&fixture, run.state_path())?;
        let active = activation(run.state_path(), None)?;
        require_uncertain(run.complete_run_with_fault(SESSION, Some(completion_fault(fault)))?)?;
        completion(run.state_path(), &active)?;
        assert_legacy_writer_held(&state, &key)?;
        assert!(state.acquire_resume_run(&key, SESSION).is_err());
        let before = fixture.snapshot()?;
        require_completed(run.reconcile_completed()?)?;
        assert_eq!(fixture.snapshot()?, before);
        stable.assert_preserved(run.state_path(), MUTATION_A)?;
        assert_legacy_writer_held(&state, &key)?;
        drop(run);
        let resumed = state.acquire_resume_run(&key, SESSION)?;
        stable.assert_preserved(resumed.state_path(), MUTATION_A)?;
        assert_ne!(
            activation(resumed.state_path(), Some(SESSION))?.get("run_id"),
            active.get("run_id")
        );
    }
    Ok(())
}

#[test]
fn restart_accepts_visible_completion_but_missing_completion_quarantines_state() -> TestResult {
    for lose_completion in [false, true] {
        let fixture = Fixture::new()?;
        let state = fixture.state()?;
        let key = fixture.key(NativeSdkProvider::Codex)?;
        let mut run = state.acquire_first_run(&key)?;
        private_file(&run.state_path().join("sdk-mutation"), MUTATION_A)?;
        let stable = StableState::capture(&fixture, run.state_path())?;
        let active = activation(run.state_path(), None)?;
        require_uncertain(
            run.complete_run_with_fault(SESSION, Some(NativeSdkRunFsFault::AfterCompletionRename))?,
        )?;
        if lose_completion {
            fs::remove_file(run_path(run.state_path(), COMPLETED)?)?;
        }
        drop(run);
        drop(state);
        let later = fixture.state()?;
        if lose_completion {
            let before = fixture.snapshot()?;
            assert!(later.acquire_resume_run(&key, SESSION).is_err());
            assert_eq!(fixture.snapshot()?, before);
            stable.assert_preserved(&stable.state_path, MUTATION_A)?;
        } else {
            let resumed = later.acquire_resume_run(&key, SESSION)?;
            stable.assert_preserved(resumed.state_path(), MUTATION_A)?;
            assert_ne!(
                activation(resumed.state_path(), Some(SESSION))?.get("run_id"),
                active.get("run_id")
            );
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug)]
enum RecordEdit {
    Malformed,
    Unknown,
    Schema,
    MissingSchema,
    MissingRun,
    MissingOwner,
    MissingRequested,
    Oversized,
    InvalidUuid,
    OtherRun,
    InvalidRequested,
    OtherRequested,
    OtherSession,
    MissingSession,
    InvalidSession,
    OtherGeneration,
    OtherProject,
    OtherProvider,
    OtherModel,
    OtherPhysicalOwner,
}

fn edited_record(bytes: &[u8], edit: RecordEdit) -> TestResult<Vec<u8>> {
    if matches!(edit, RecordEdit::Malformed) {
        return Ok(b"{invalid JSON".to_vec());
    }
    if matches!(edit, RecordEdit::Oversized) {
        let mut oversized = bytes.to_vec();
        oversized.resize(bytes.len().max(65537), b' ');
        return Ok(oversized);
    }
    let mut value: Value = serde_json::from_slice(bytes)?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| io::Error::other("fixture record is not an object"))?;
    match edit {
        RecordEdit::Unknown => {
            object.insert("unexpected".into(), json!(true));
        }
        RecordEdit::Schema => {
            object.insert("schema_version".into(), json!(2));
        }
        RecordEdit::MissingSchema => {
            object.remove("schema_version");
        }
        RecordEdit::MissingRun => {
            object.remove("run_id");
        }
        RecordEdit::MissingOwner => {
            object.remove("owner");
        }
        RecordEdit::MissingRequested => {
            object.remove("requested_session");
        }
        RecordEdit::InvalidUuid => {
            object.insert("run_id".into(), json!("not-a-uuid"));
        }
        RecordEdit::OtherRun => {
            object.insert("run_id".into(), json!(Uuid::new_v4()));
        }
        RecordEdit::InvalidRequested => {
            object.insert("requested_session".into(), json!(" invalid-session"));
        }
        RecordEdit::OtherRequested => {
            object.insert("requested_session".into(), json!("other-request"));
        }
        RecordEdit::OtherSession => {
            object.insert("session_id".into(), json!("other-ack"));
        }
        RecordEdit::MissingSession => {
            object.remove("session_id");
        }
        RecordEdit::InvalidSession => {
            object.insert("session_id".into(), json!(" invalid-ack"));
        }
        RecordEdit::OtherGeneration => {
            value["owner"]["generation"] = json!(Uuid::new_v4());
        }
        RecordEdit::OtherProject => {
            value["owner"]["key"]["project"] = json!("/a-foreign-project-not-used-as-a-fixture");
        }
        RecordEdit::OtherProvider => {
            value["owner"]["key"]["provider"] = json!("claude");
        }
        RecordEdit::OtherModel => {
            value["owner"]["key"]["model"] = json!("other-model");
        }
        RecordEdit::OtherPhysicalOwner => {
            value["owner"]["state_dir"]["inode"] = json!(0);
        }
        RecordEdit::Malformed | RecordEdit::Oversized => unreachable!(),
    }
    Ok(serde_json::to_vec(&value)?)
}

#[test]
fn named_active_must_match_before_any_association_or_completion_write() -> TestResult {
    for edit in [
        None,
        Some(RecordEdit::OtherRun),
        Some(RecordEdit::OtherRequested),
        Some(RecordEdit::OtherGeneration),
    ] {
        let fixture = Fixture::new()?;
        let state = fixture.state()?;
        let key = fixture.key(NativeSdkProvider::Codex)?;
        let mut run = state.acquire_first_run(&key)?;
        private_file(&run.state_path().join("sdk-mutation"), MUTATION_A)?;
        let active = run_path(run.state_path(), ACTIVE)?;
        let original = fs::read(&active)?;
        if let Some(edit) = edit {
            overwrite(&active, &edited_record(&original, edit)?)?;
        } else {
            fs::remove_file(&active)?;
        }
        let before = fixture.snapshot()?;
        assert!(run.complete_run(SESSION).is_err());
        assert_eq!(fixture.snapshot()?, before);
        assert!(!state.association_path(&key, SESSION)?.exists());
        assert!(!run_path(run.state_path(), COMPLETED)?.exists());
        if edit.is_some() {
            overwrite(&active, &original)?;
        } else {
            private_file(&active, &original)?;
        }
        require_completed(run.complete_run(SESSION)?)?;
    }
    Ok(())
}

#[test]
fn exchanged_active_file_cannot_use_cached_proof_to_publish() -> TestResult {
    let fixture = Fixture::new()?;
    let state = fixture.state()?;
    let key = fixture.key(NativeSdkProvider::Codex)?;
    let mut run = state.acquire_first_run(&key)?;
    private_file(&run.state_path().join("sdk-mutation"), MUTATION_A)?;
    let active = run_path(run.state_path(), ACTIVE)?;
    let saved = active.with_extension("saved");
    let original = fs::read(&active)?;
    fs::rename(&active, &saved)?;
    private_file(&active, &edited_record(&original, RecordEdit::OtherRun)?)?;
    let before = fixture.snapshot()?;
    assert!(run.complete_run(SESSION).is_err());
    assert_eq!(fixture.snapshot()?, before);
    assert!(!state.association_path(&key, SESSION)?.exists());
    assert!(!run_path(run.state_path(), COMPLETED)?.exists());
    fs::remove_file(&active)?;
    fs::rename(&saved, &active)?;
    require_completed(run.complete_run(SESSION)?)?;
    Ok(())
}

#[test]
fn uncertain_phase_is_sticky_until_exact_reconciliation_and_completed_phase_never_rewrites(
) -> TestResult {
    for lose_proof in [false, true] {
        let fixture = Fixture::new()?;
        let state = fixture.state()?;
        let key = fixture.key(NativeSdkProvider::Codex)?;
        let mut run = state.acquire_first_run(&key)?;
        private_file(&run.state_path().join("sdk-mutation"), MUTATION_A)?;
        require_uncertain(
            run.complete_run_with_fault(SESSION, Some(NativeSdkRunFsFault::AfterCompletionRename))?,
        )?;
        let before = fixture.snapshot()?;
        assert!(run.complete_run(SESSION).is_err());
        assert_eq!(fixture.snapshot()?, before);
        let path = run_path(run.state_path(), COMPLETED)?;
        let original = fs::read(&path)?;
        if lose_proof {
            fs::remove_file(&path)?;
        } else {
            overwrite(&path, &edited_record(&original, RecordEdit::OtherRun)?)?;
        }
        let damaged = fixture.snapshot()?;
        require_not_completed(run.reconcile_completed())?;
        assert_eq!(fixture.snapshot()?, damaged);
        assert!(run.complete_run(SESSION).is_err());
        assert_eq!(fixture.snapshot()?, damaged);
        if lose_proof {
            private_file(&path, &original)?;
        } else {
            overwrite(&path, &original)?;
        }
        let restored = fixture.snapshot()?;
        require_completed(run.reconcile_completed()?)?;
        assert_eq!(fixture.snapshot()?, restored);
        assert!(run.complete_run(SESSION).is_err());
        assert_eq!(fixture.snapshot()?, restored);
        assert!(run.reconcile_completed().is_err());
        assert_eq!(fixture.snapshot()?, restored);
    }
    Ok(())
}

#[test]
fn active_reconciliation_cannot_manufacture_completion_from_missing_or_old_proof() -> TestResult {
    for resumed in [false, true] {
        let fixture = Fixture::new()?;
        let (state, key, mut run) = if resumed {
            let established = establish(&fixture)?;
            let run = established
                .state
                .acquire_resume_run(&established.key, SESSION)?;
            (established.state, established.key, run)
        } else {
            let state = fixture.state()?;
            let key = fixture.key(NativeSdkProvider::Codex)?;
            let run = state.acquire_first_run(&key)?;
            (state, key, run)
        };
        let before = fixture.snapshot()?;
        assert!(run.reconcile_completed().is_err());
        assert_eq!(fixture.snapshot()?, before);
        require_completed(run.complete_run(SESSION)?)?;
        assert_legacy_writer_held(&state, &key)?;
    }
    Ok(())
}

#[test]
fn reconciliation_faults_reject_rename_events_and_preserve_uncertainty_after_real_sync(
) -> TestResult {
    let fixture = Fixture::new()?;
    let state = fixture.state()?;
    let key = fixture.key(NativeSdkProvider::Codex)?;
    let mut run = state.acquire_first_run(&key)?;
    private_file(&run.state_path().join("sdk-mutation"), MUTATION_A)?;
    require_uncertain(
        run.complete_run_with_fault(SESSION, Some(NativeSdkRunFsFault::AfterCompletionRename))?,
    )?;
    let before = fixture.snapshot()?;
    for fault in [
        NativeSdkRunFsFault::AfterActivationRename,
        NativeSdkRunFsFault::AfterAssociationRename,
        NativeSdkRunFsFault::AfterCompletionRename,
    ] {
        assert!(run.reconcile_completed_with_fault(Some(fault)).is_err());
        assert_eq!(fixture.snapshot()?, before);
    }
    require_not_completed(
        run.reconcile_completed_with_fault(Some(NativeSdkRunFsFault::AfterCompletionDirectorySync)),
    )?;
    assert_eq!(fixture.snapshot()?, before);
    assert!(run.complete_run(SESSION).is_err());
    assert_eq!(fixture.snapshot()?, before);
    require_completed(run.reconcile_completed()?)?;
    assert_eq!(fixture.snapshot()?, before);
    Ok(())
}

#[test]
fn reconciliation_rereads_each_exact_record_and_retains_failed_phase_until_proof_restored(
) -> TestResult {
    for record in 0..3 {
        let edits: Vec<Option<RecordEdit>> = if record == 2 {
            vec![
                None,
                Some(RecordEdit::OtherSession),
                Some(RecordEdit::OtherGeneration),
                Some(RecordEdit::Schema),
                Some(RecordEdit::Unknown),
            ]
        } else {
            vec![
                None,
                Some(RecordEdit::OtherRun),
                Some(RecordEdit::OtherRequested),
                Some(RecordEdit::OtherGeneration),
                Some(RecordEdit::OtherPhysicalOwner),
                Some(RecordEdit::Malformed),
            ]
        };
        for edit in edits {
            let fixture = Fixture::new()?;
            let state = fixture.state()?;
            let key = fixture.key(NativeSdkProvider::Codex)?;
            let mut run = state.acquire_first_run(&key)?;
            private_file(&run.state_path().join("sdk-mutation"), MUTATION_A)?;
            require_uncertain(run.complete_run_with_fault(
                SESSION,
                Some(NativeSdkRunFsFault::AfterCompletionRename),
            )?)?;
            let target = match record {
                0 => run_path(run.state_path(), ACTIVE)?,
                1 => run_path(run.state_path(), COMPLETED)?,
                _ => state.association_path(&key, SESSION)?,
            };
            let original = fs::read(&target)?;
            if let Some(edit) = edit {
                overwrite(&target, &edited_record(&original, edit)?)?;
            } else {
                fs::remove_file(&target)?;
            }
            let before = fixture.snapshot()?;
            require_not_completed(run.reconcile_completed())?;
            assert_eq!(
                fixture.snapshot()?,
                before,
                "reconciliation wrote after damage to record {record}, edit {edit:?}"
            );
            assert!(run.complete_run(SESSION).is_err());
            assert_eq!(fixture.snapshot()?, before);
            if edit.is_some() {
                overwrite(&target, &original)?;
            } else {
                private_file(&target, &original)?;
            }
            let restored = fixture.snapshot()?;
            require_completed(run.reconcile_completed()?)?;
            assert_eq!(fixture.snapshot()?, restored);
        }
    }
    Ok(())
}

#[test]
fn strict_run_records_reject_malformed_missing_nullable_foreign_and_oversized_fields() -> TestResult
{
    let edits = [
        RecordEdit::Malformed,
        RecordEdit::Unknown,
        RecordEdit::Schema,
        RecordEdit::MissingSchema,
        RecordEdit::MissingRun,
        RecordEdit::MissingOwner,
        RecordEdit::MissingRequested,
        RecordEdit::Oversized,
        RecordEdit::InvalidUuid,
        RecordEdit::OtherRun,
        RecordEdit::InvalidRequested,
        RecordEdit::OtherRequested,
        RecordEdit::OtherGeneration,
        RecordEdit::OtherProject,
        RecordEdit::OtherProvider,
        RecordEdit::OtherModel,
        RecordEdit::OtherPhysicalOwner,
    ];
    for name in [ACTIVE, COMPLETED] {
        for edit in edits {
            let fixture = Fixture::new()?;
            let established = establish(&fixture)?;
            let target = run_path(&established.stable.state_path, name)?;
            let original = fs::read(&target)?;
            overwrite(&target, &edited_record(&original, edit)?)?;
            let before = fixture.snapshot()?;
            assert!(
                established
                    .state
                    .acquire_resume_run(&established.key, SESSION)
                    .is_err(),
                "accepted {name} edit {edit:?}"
            );
            assert_eq!(fixture.snapshot()?, before);
            established
                .stable
                .assert_preserved(&established.stable.state_path, MUTATION_A)?;
            overwrite(&target, &original)?;
            let restored = established
                .state
                .acquire_resume_run(&established.key, SESSION)?;
            established
                .stable
                .assert_preserved(restored.state_path(), MUTATION_A)?;
        }
    }
    for edit in [
        RecordEdit::OtherSession,
        RecordEdit::MissingSession,
        RecordEdit::InvalidSession,
    ] {
        let fixture = Fixture::new()?;
        let established = establish(&fixture)?;
        let target = run_path(&established.stable.state_path, COMPLETED)?;
        overwrite(&target, &edited_record(&fs::read(&target)?, edit)?)?;
        let before = fixture.snapshot()?;
        assert!(established
            .state
            .acquire_resume_run(&established.key, SESSION)
            .is_err());
        assert_eq!(fixture.snapshot()?, before);
    }
    Ok(())
}

#[test]
fn strict_run_records_reject_unsafe_links_modes_missing_and_exchanged_files_without_rebinding(
) -> TestResult {
    for name in [ACTIVE, COMPLETED] {
        for damage in 0..5 {
            let fixture = Fixture::new()?;
            let established = establish(&fixture)?;
            let target = run_path(&established.stable.state_path, name)?;
            let saved = target.with_extension("saved");
            match damage {
                0 => fs::set_permissions(&target, fs::Permissions::from_mode(0o644))?,
                1 => {
                    fs::rename(&target, &saved)?;
                    symlink(&saved, &target)?;
                }
                2 => fs::hard_link(&target, &saved)?,
                3 => {
                    let bytes = fs::read(&target)?;
                    fs::rename(&target, &saved)?;
                    private_file(&target, &edited_record(&bytes, RecordEdit::OtherRun)?)?;
                }
                _ => fs::rename(&target, &saved)?,
            }
            let before = fixture.snapshot()?;
            assert!(
                established
                    .state
                    .acquire_resume_run(&established.key, SESSION)
                    .is_err(),
                "accepted {name} filesystem damage {damage}"
            );
            assert_eq!(fixture.snapshot()?, before);
            match damage {
                0 => fs::set_permissions(&target, fs::Permissions::from_mode(0o600))?,
                2 => fs::remove_file(&saved)?,
                _ => {
                    if damage != 4 {
                        fs::remove_file(&target)?;
                    }
                    fs::rename(&saved, &target)?;
                }
            }
            let restored = established
                .state
                .acquire_resume_run(&established.key, SESSION)?;
            established
                .stable
                .assert_preserved(restored.state_path(), MUTATION_A)?;
        }
    }
    Ok(())
}

#[test]
fn strict_resume_rejects_exchanged_original_owner_state_lock_and_directory_pins() -> TestResult {
    for target_index in [0, 1, 2, 3, 4, 5, 6, 7, 8, 9] {
        let fixture = Fixture::new()?;
        let established = establish(&fixture)?;
        let owner = owner_path(&established.stable.state_path)?;
        let owners = owner
            .parent()
            .ok_or_else(|| io::Error::other("owner missing parent"))?;
        let namespace = owners
            .parent()
            .ok_or_else(|| io::Error::other("owners missing namespace"))?;
        let targets = [
            fixture.project.clone(),
            fixture.paths.root().to_path_buf(),
            fixture.paths.root().join("native-sdk-state"),
            namespace.to_path_buf(),
            owners.to_path_buf(),
            namespace.join("sessions"),
            owner.clone(),
            established.stable.state_path.clone(),
            owner.join("owner.json"),
            owner.join("writer.lock"),
        ];
        let target = &targets[target_index];
        let saved = target.with_extension("saved");
        let directory = fs::symlink_metadata(target)?.is_dir();
        fs::rename(target, &saved)?;
        if directory {
            private_dir(target)?;
        } else {
            private_file(target, b"")?;
        }
        let before = fixture.snapshot()?;
        assert!(
            established
                .state
                .acquire_resume_run(&established.key, SESSION)
                .is_err(),
            "accepted exchanged physical pin {target_index}"
        );
        assert_eq!(fixture.snapshot()?, before);
        if directory {
            fs::remove_dir(target)?;
        } else {
            fs::remove_file(target)?;
        }
        fs::rename(&saved, target)?;
        let restored = established
            .state
            .acquire_resume_run(&established.key, SESSION)?;
        established
            .stable
            .assert_preserved(restored.state_path(), MUTATION_A)?;
    }
    Ok(())
}

#[test]
fn reconciliation_rejects_unsafe_named_records_without_losing_original_writer_custody() -> TestResult
{
    for record in 0..3 {
        for damage in 0..3 {
            let fixture = Fixture::new()?;
            let state = fixture.state()?;
            let key = fixture.key(NativeSdkProvider::Codex)?;
            let mut run = state.acquire_first_run(&key)?;
            private_file(&run.state_path().join("sdk-mutation"), MUTATION_A)?;
            require_uncertain(run.complete_run_with_fault(
                SESSION,
                Some(NativeSdkRunFsFault::AfterCompletionRename),
            )?)?;
            let target = match record {
                0 => run_path(run.state_path(), ACTIVE)?,
                1 => run_path(run.state_path(), COMPLETED)?,
                _ => state.association_path(&key, SESSION)?,
            };
            let saved = target.with_extension("saved");
            match damage {
                0 => fs::set_permissions(&target, fs::Permissions::from_mode(0o644))?,
                1 => {
                    fs::rename(&target, &saved)?;
                    symlink(&saved, &target)?;
                }
                _ => fs::hard_link(&target, &saved)?,
            }
            let before = fixture.snapshot()?;
            require_not_completed(run.reconcile_completed())?;
            assert_eq!(fixture.snapshot()?, before);
            assert!(run.complete_run(SESSION).is_err());
            assert_eq!(fixture.snapshot()?, before);
            match damage {
                0 => fs::set_permissions(&target, fs::Permissions::from_mode(0o600))?,
                1 => {
                    fs::remove_file(&target)?;
                    fs::rename(&saved, &target)?;
                }
                _ => fs::remove_file(&saved)?,
            }
            assert_legacy_writer_held(&state, &key)?;
            let restored = fixture.snapshot()?;
            require_completed(run.reconcile_completed()?)?;
            assert_eq!(fixture.snapshot()?, restored);
        }
    }
    Ok(())
}

#[test]
fn reconciliation_rejects_exchanged_physical_pins_and_never_adopts_replacement_lock() -> TestResult
{
    for target_index in [0, 1, 2, 3, 4, 5, 6, 7, 8, 9] {
        let fixture = Fixture::new()?;
        let state = fixture.state()?;
        let key = fixture.key(NativeSdkProvider::Codex)?;
        let mut run = state.acquire_first_run(&key)?;
        private_file(&run.state_path().join("sdk-mutation"), MUTATION_A)?;
        let stable = StableState::capture(&fixture, run.state_path())?;
        require_uncertain(
            run.complete_run_with_fault(SESSION, Some(NativeSdkRunFsFault::AfterCompletionRename))?,
        )?;
        let owner = owner_path(run.state_path())?;
        let owners = owner
            .parent()
            .ok_or_else(|| io::Error::other("owner missing parent"))?;
        let namespace = owners
            .parent()
            .ok_or_else(|| io::Error::other("owners missing namespace"))?;
        let targets = [
            fixture.project.clone(),
            fixture.paths.root().to_path_buf(),
            fixture.paths.root().join("native-sdk-state"),
            namespace.to_path_buf(),
            owners.to_path_buf(),
            namespace.join("sessions"),
            owner.clone(),
            run.state_path().to_path_buf(),
            owner.join("owner.json"),
            owner.join("writer.lock"),
        ];
        let target = &targets[target_index];
        let saved = target.with_extension("saved");
        let directory = fs::symlink_metadata(target)?.is_dir();
        fs::rename(target, &saved)?;
        if directory {
            private_dir(target)?;
        } else {
            private_file(target, b"")?;
        }
        let before = fixture.snapshot()?;
        require_not_completed(run.reconcile_completed())?;
        assert_eq!(fixture.snapshot()?, before);
        assert!(run.complete_run(SESSION).is_err());
        assert_eq!(fixture.snapshot()?, before);
        if directory {
            fs::remove_dir(target)?;
        } else {
            fs::remove_file(target)?;
        }
        fs::rename(&saved, target)?;
        assert_legacy_writer_held(&state, &key)?;
        let restored = fixture.snapshot()?;
        require_completed(run.reconcile_completed()?)?;
        assert_eq!(fixture.snapshot()?, restored);
        stable.assert_preserved(run.state_path(), MUTATION_A)?;
    }
    Ok(())
}
