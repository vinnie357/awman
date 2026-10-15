use super::*;
use crate::engine::container::apple_initial_inspection_bounds_test::StartedInspectionFixture;
use std::error::Error;
use std::io;
use std::os::unix::process::ExitStatusExt as _;

struct OwnedTestChild(Option<Child>);

impl Drop for OwnedTestChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            if !matches!(child.try_wait(), Ok(Some(_))) {
                if let Err(error) = child.kill() {
                    eprintln!("deadline control cleanup kill failed: {error}");
                }
                if let Err(error) = child.wait() {
                    eprintln!("deadline control cleanup wait failed: {error}");
                }
            }
        }
    }
}

struct SupervisorCustody {
    actor: Arc<ActorControl>,
    worker: Option<JoinHandle<()>>,
}

impl SupervisorCustody {
    fn join(&mut self) -> Result<(), Box<dyn Error>> {
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| io::Error::other("deadline supervisor worker panicked"))?;
        }
        Ok(())
    }
}

impl Drop for SupervisorCustody {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            if !worker.is_finished() {
                self.actor.request_terminate();
            }
            if worker.join().is_err() {
                eprintln!("deadline supervisor cleanup worker panicked");
            }
        }
    }
}

#[test]
fn expired_deadline_kills_and_reaps_known_live_child_before_fixture_release(
) -> Result<(), Box<dyn Error>> {
    let mut fixture = StartedInspectionFixture::new()?;
    let expected_pid = fixture.pid;
    let actor = Arc::new(ActorControl::new());
    let worker_actor = Arc::clone(&actor);
    let (child_tx, child_rx) = mpsc::sync_channel::<OwnedTestChild>(1);
    let (stdout_tx, stdout_rx) = mpsc::channel();
    let (stderr_tx, stderr_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::sync_channel(1);
    let child = OwnedTestChild(Some(fixture.take_child()?));
    let expired_deadline = Instant::now();
    let worker = thread::Builder::new().spawn(move || {
        if let Ok(mut owned) = child_rx.recv() {
            if let Some(child) = owned.0.take() {
                supervise_child(
                    child,
                    expired_deadline,
                    worker_actor,
                    stdout_rx,
                    stderr_rx,
                    None,
                );
                let _ = done_tx.send(());
            }
        }
    })?;
    let mut custody = SupervisorCustody {
        actor: Arc::clone(&actor),
        worker: Some(worker),
    };
    let invoked = Instant::now();
    let observation_deadline = invoked + MAX_PROVIDER_CUSTODY_SHUTDOWN;
    child_tx
        .send(child)
        .map_err(|_| io::Error::other("deadline supervisor did not receive the real Child"))?;
    drop(child_tx);
    done_rx
        .recv_timeout(observation_deadline.saturating_duration_since(Instant::now()))
        .map_err(|_| {
            "known-live deadline supervisor did not return within the custody allowance"
        })?;
    custody.join()?;
    let elapsed = invoked.elapsed();
    let failure = {
        let mut terminal = lock_unpoisoned(&actor.shared);
        match (
            terminal.initial.take(),
            terminal.retained_reaped.take(),
            terminal.retained_not_started.take(),
        ) {
            (Some(ActorInitialResult::Reaped(failure)), None, None)
            | (None, Some(failure), None) => failure,
            _ => panic!("known-live supervisor omitted its unique actual-reap terminal"),
        }
    };

    assert!(elapsed <= MAX_PROVIDER_CUSTODY_SHUTDOWN);
    assert_eq!(failure.kind, ProviderCliReapedFailureKind::DeadlineExceeded);
    assert_eq!(failure.child_pid, expected_pid);
    assert_ne!(failure.child_pid, 0);
    assert!(!failure.status.success());
    assert_eq!(failure.status.signal(), Some(nix::libc::SIGKILL));
    assert_eq!(
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(i32::try_from(expected_pid)?),
            None
        ),
        Err(nix::errno::Errno::ESRCH),
        "the actual Child must be reaped while RELEASE remains held"
    );
    assert_eq!(actor.termination_generation(), 0);
    drop((stdout_tx, stderr_tx));
    drop(fixture);
    Ok(())
}
