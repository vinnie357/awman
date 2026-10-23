//! A2b.2: exchanged physical sources never gain original main release authority.
use super::native_codex_auth_entry_test::command;
use super::native_codex_main_entry_marker_test::{prove_entry_control, two_streams};
use super::Command;
use crate::command::error::CommandError;
use crate::engine::agent::native_codex_auth_preparation_test::TestResult;
use crate::engine::agent::native_codex_main_a2a_test_support::{
    is_main, A2Fixture, ObservedBoundary,
};
use crate::engine::agent::native_codex_main_a2m_test_support::instrument;
use crate::engine::agent::native_codex_main_entry_marker_support::install;
use crate::engine::container::gated_launch::*;
use crate::engine::error::EngineError;
use serde_json::Value;
use std::fs::{self, OpenOptions};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

struct KeptExchange {
    _fixture: A2Fixture,
    _registry: Arc<LaunchRetentionRegistry>,
    _probe: retention_test_support::RetentionProbe,
    _ticket: Option<LaunchRetentionTicket>,
    _claims: retention_test_support::SupervisorActiveClaimBarrier,
}
static KEPT: OnceLock<Mutex<Vec<KeptExchange>>> = OnceLock::new();
struct Original {
    target: PathBuf,
    held: PathBuf,
    identity: (u64, u64),
    owner: PathBuf,
    controls: PathBuf,
    obsolete_controls: PathBuf,
    owner_bytes: Vec<u8>,
    activation: Value,
    lock_identity: (u64, u64),
    exited_before_exchange: usize,
}
fn identity(path: &Path) -> TestResult<(u64, u64)> {
    let metadata = fs::symlink_metadata(path)?;
    assert!(!metadata.file_type().is_symlink());
    Ok((metadata.dev(), metadata.ino()))
}
fn rebase(path: &Path, target: &Path, held: &Path) -> PathBuf {
    path.strip_prefix(target)
        .map_or_else(|_| path.to_owned(), |suffix| held.join(suffix))
}
fn assert_original(e: &Original) -> TestResult {
    assert_eq!(identity(&e.held)?, e.identity);
    assert_ne!(identity(&e.target)?, e.identity);
    assert_eq!(
        fs::read(e.target.join("replacement-sentinel"))?,
        b"replacement is not owned cleanup"
    );
    assert_eq!(fs::read_dir(&e.target)?.count(), 1);
    assert_eq!(fs::read(e.owner.join("owner.json"))?, e.owner_bytes);
    let active: Value = serde_json::from_slice(&fs::read(e.owner.join("active-run.json"))?)?;
    assert_eq!(active, e.activation);
    assert!(!e.owner.join("completed-run.json").exists());
    for controls in [&e.controls, &e.obsolete_controls] {
        assert!(!controls.join("release.json").exists());
        assert!(!controls.join(".released").exists());
    }
    let lock_path = e.owner.join("writer.lock");
    assert_eq!(identity(&lock_path)?, e.lock_identity);
    Ok(())
}
fn assert_lock(e: &Original, retained: bool) -> TestResult {
    let lock_path = e.owner.join("writer.lock");
    assert_eq!(identity(&lock_path)?, e.lock_identity);
    let lock = OpenOptions::new().read(true).write(true).open(lock_path)?;
    let metadata = lock.metadata()?;
    assert_eq!((metadata.dev(), metadata.ino()), e.lock_identity);
    let result = lock.try_lock();
    drop(lock);
    if retained {
        assert!(matches!(result, Err(std::fs::TryLockError::WouldBlock)));
    } else {
        assert!(
            result.is_ok(),
            "retired failed main still owns original writer lock"
        );
    }
    Ok(())
}
fn resolved_main(
    probe: &retention_test_support::RetentionProbe,
    ticket: LaunchRetentionTicket,
    boundary: &ObservedBoundary,
) -> bool {
    let snapshots = boundary.snapshots.lock().expect("actual main retirement");
    let witnessed = snapshots
        .last()
        .is_some_and(|main| is_main(&main.argv) && main.reaped && main.cleaned && main.released);
    let state = probe.snapshot();
    witnessed
        && probe.resource_loan_releases(ticket) == 1
        && !probe.contains(ticket)
        && probe.only_ticket().is_none()
        && state.retained == 0
        && state.unreaped == 0
        && probe.active_gated_resource_loans(ticket) == 0
}
fn assert_retained(probe: &retention_test_support::RetentionProbe, ticket: LaunchRetentionTicket) {
    assert!(probe.contains(ticket));
    assert_eq!(probe.only_ticket(), Some(ticket));
    assert_eq!(probe.snapshot().retained, 1);
    assert_eq!(probe.snapshot().unreaped, 0);
}
async fn run_exchange(data_root: bool) -> TestResult {
    prove_entry_control().await?;
    let (fixture, release, _entered) = A2Fixture::new(true)?;
    let mut fixture = fixture;
    let stable = fs::canonicalize(fixture.fixture.auth.path())?;
    let project = fs::canonicalize(fixture.fixture.auth.session.git_root())?;
    let target = if data_root {
        stable.join("config/data")
    } else {
        project.clone()
    };
    let held = stable.join(if data_root {
        "held-data"
    } else {
        "held-project"
    });
    let saved = Arc::new(Mutex::new(None::<Original>));
    let record = Arc::clone(&saved);
    let snapshots = Arc::clone(&fixture.boundary);
    let observer = Arc::clone(&fixture.fixture.auth.observer);
    let probe_slot = Arc::new(Mutex::new(None::<retention_test_support::RetentionProbe>));
    let observation_probe = Arc::clone(&probe_slot);
    let scope = instrument(&mut fixture)?;
    let entry = install(
        &mut fixture,
        scope,
        Box::new(move |argv| {
            let snapshots = snapshots.snapshots.lock().expect("actual bound snapshots");
            assert_eq!(snapshots.len(), 2);
            let (auth, main) = (&snapshots[0], &snapshots[1]);
            assert!(!is_main(&auth.argv) && is_main(argv));
            assert_eq!(main.argv, argv);
            assert!(auth.reaped && auth.cleaned && auth.released);
            assert_eq!(main.owner, auth.owner);
            assert_eq!(main.active, auth.active);
            let sdk = observer.state_path();
            let mounts = argv
                .windows(2)
                .filter(|p| p[0] == "-v")
                .map(|p| p[1].as_str())
                .collect::<Vec<_>>();
            assert_eq!(mounts.len(), 4);
            assert_eq!(
                mounts
                    .iter()
                    .filter(|m| **m == format!("{}:/workspace:ro", project.display()))
                    .count(),
                1
            );
            assert_eq!(
                mounts
                    .iter()
                    .filter(|m| **m == format!("{}:/data/awman-native-sdk/codex", sdk.display()))
                    .count(),
                1
            );
            assert_eq!(
                mounts
                    .iter()
                    .filter(|m| m.ends_with(":/.awman/startup-gate/bin:ro"))
                    .count(),
                1
            );
            assert_eq!(
                mounts
                    .iter()
                    .filter(|m| **m
                        == format!("{}:/.awman/startup-gate/control", main.controls.display()))
                    .count(),
                1
            );
            let owner = sdk.parent().expect("original owner").to_owned();
            let original = Original {
                identity: identity(&target).expect("original physical source"),
                owner: rebase(&owner, &target, &held),
                controls: rebase(&main.controls, &target, &held),
                obsolete_controls: main.controls.clone(),
                owner_bytes: main.owner.clone(),
                activation: main.active.clone(),
                lock_identity: identity(&owner.join("writer.lock")).expect("original lock"),
                exited_before_exchange: observation_probe
                    .lock()
                    .expect("registry observation")
                    .as_ref()
                    .expect("registry installed before command")
                    .snapshot()
                    .managed_exited,
                target: target.clone(),
                held: held.clone(),
            };
            assert!(!main.controls.join("release.json").exists());
            assert!(!main.controls.join(".released").exists());
            fs::rename(&target, &held).expect("hold original physical source");
            fs::create_dir(&target).expect("replacement physical directory");
            fs::write(
                target.join("replacement-sentinel"),
                b"replacement is not owned cleanup",
            )
            .expect("replacement sentinel");
            assert!(record
                .lock()
                .expect("one exchange")
                .replace(original)
                .is_none());
        }),
    )?;
    let registry = fixture.fixture.auth.runtime.launch_retention()?;
    let probe = retention_test_support::registry_probe(&registry);
    *probe_slot.lock().expect("install observation probe") = Some(probe.clone());
    let claims = probe.hold_supervisor_active_claims();
    assert!(probe.only_ticket().is_none());
    release.release();
    let (frontend, _outputs) = two_streams(&fixture.fixture.auth);
    let command = command(&fixture.fixture.auth);
    let boundary = Arc::clone(&fixture.boundary);
    let keeper_index = {
        let mut kept = KEPT
            .get_or_init(|| Mutex::new(Vec::new()))
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let index = kept.len();
        kept.push(KeptExchange {
            _fixture: fixture,
            _registry: Arc::clone(&registry),
            _probe: probe.clone(),
            _ticket: None,
            _claims: claims,
        });
        index
    };
    let result =
        tokio::time::timeout(Duration::from_secs(15), command.run_with_frontend(frontend)).await?;
    let ticket = probe.only_ticket().ok_or("original main custody ticket")?;
    {
        let mut kept = KEPT
            .get()
            .expect("original fixture keeper")
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        kept[keeper_index]._ticket = Some(ticket);
    }
    assert!(
        matches!(result, Err(CommandError::Engine(_))),
        "exchange did not reject: {result:?}"
    );
    let exited_before = saved
        .lock()
        .expect("actual exchange")
        .as_ref()
        .ok_or("physical exchange never ran")?
        .exited_before_exchange;
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            probe.advance_one_supervisor_active_attempt()?;
            let state = probe.snapshot();
            let started = entry.main_started.load(Ordering::Acquire);
            let reaped = entry.main_reaped.load(Ordering::Acquire)
                || (state.managed_exited > exited_before && state.unreaped == 0);
            let never = entry.main_never_created.load(Ordering::Acquire);
            if (started && reaped && !never) || (!started && never) {
                break Ok::<(), EngineError>(());
            }
            tokio::task::yield_now().await;
        }
    })
    .await??;
    {
        let state = saved.lock().expect("exchange observation");
        assert_original(state.as_ref().ok_or("physical exchange never ran")?)?;
    }
    assert!(fs::read_dir(&entry.marker_dir)?.next().is_none());
    let resolved = !data_root && resolved_main(&probe, ticket, &boundary);
    if !resolved {
        assert_retained(&probe, ticket);
        {
            let state = saved.lock().expect("unresolved original authority");
            assert_lock(state.as_ref().ok_or("unresolved physical source")?, true)?;
        }
        let retry = registry.retry_active_gated(ticket, Instant::now() + Duration::from_secs(15));
        if !data_root && matches!(&retry, Ok(ActiveGatedRetryOutcome::Resolved)) {
            assert!(resolved_main(&probe, ticket, &boundary));
        } else {
            let retained = match &retry {
                Ok(ActiveGatedRetryOutcome::Retained) => true,
                Err(EngineError::Container(message)) => {
                    message == "retained launch ticket is not an active gated launch"
                }
                _ => false,
            };
            assert!(retained, "unexpected original-custody retry outcome");
            assert_retained(&probe, ticket);
        }
    }
    {
        let state = saved.lock().expect("post-disposition original evidence");
        let original = state.as_ref().ok_or("original physical source")?;
        assert_original(original)?;
        let resolved = !data_root && resolved_main(&probe, ticket, &boundary);
        if !resolved {
            assert_retained(&probe, ticket);
        }
        assert_lock(original, !resolved)?;
    }
    assert!(fs::read_dir(&entry.marker_dir)?.next().is_none());
    Ok(())
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exchanged_physical_workspace_denies_main_and_preserves_original_custody() -> TestResult {
    run_exchange(false).await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exchanged_physical_data_root_denies_main_and_preserves_original_custody() -> TestResult {
    run_exchange(true).await
}
