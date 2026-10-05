use std::error::Error;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use super::gated_launch::child_lifecycle_test_support::{
    lifecycle_probe, queue_barrier, ActorPausePoint,
};
use super::gated_launch::retention_test_support::{registry_probe, RetentionProbe};
use super::gated_launch::{ChildLifecycleAuthority, ChildLifecycleState, LaunchRetentionRegistry};
use super::process::test_support::{
    spawn_fixture, spawn_fixture_paused, spawn_fixture_with_custody_witness, FixtureIoMode,
    FixtureSpawnFault, FixtureSpawnResult, FixtureSpawnSpec,
};

const OBSERVE: Duration = Duration::from_millis(600);
const CLEANUP: Duration = Duration::from_secs(3);

fn fixture_spec(fault: Option<FixtureSpawnFault>) -> FixtureSpawnSpec {
    FixtureSpawnSpec {
        mode: FixtureIoMode::Piped,
        executable: "/bin/sh",
        args: vec!["-c".into(), "exec sleep 5".into()],
        seeded_prompt: None,
        grace_timeout: Duration::from_millis(50),
        stuck_timeout: Duration::from_millis(50),
        gate: None,
        fault,
    }
}

fn wait_until(deadline: Instant, mut condition: impl FnMut() -> bool) -> bool {
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    condition()
}

fn authority(fixture: &FixtureSpawnResult) -> Result<ChildLifecycleAuthority, Box<dyn Error>> {
    fixture
        .slot
        .authority()
        .ok_or_else(|| "fixture did not bind lifecycle authority".into())
}

fn cleanup_child(authority: &ChildLifecycleAuthority) {
    let _ = authority.terminate_local_cli(Instant::now() + CLEANUP);
}

fn wait_for_retention_authority(probe: &RetentionProbe) -> Option<bool> {
    let deadline = Instant::now() + OBSERVE;
    let mut observation = None;
    let _ = wait_until(deadline, || {
        observation = probe.only_termination_authorized();
        observation.is_some()
    });
    observation
}

#[test]
fn prepared_attempt_without_accepted_command_preserves_retry_authority(
) -> Result<(), Box<dyn Error>> {
    let registry = LaunchRetentionRegistry::try_new()?;
    let retention = registry_probe(&registry);
    let mut task = spawn_fixture_paused(
        fixture_spec(Some(FixtureSpawnFault::AfterBindBeforeBridge)),
        Arc::clone(&registry),
        ActorPausePoint::BeforeFirstCommand,
    )?;
    let paused = task.wait_until_paused(Instant::now() + OBSERVE);
    let fixture = task
        .result_until(Instant::now() + OBSERVE)
        .ok_or("paused prepared fixture did not return")?;
    let authority = authority(&fixture)?;
    let lifecycle = lifecycle_probe(&authority);
    let retained = fixture.finish(Arc::clone(&registry)).is_err();
    let attempted_prepared = wait_until(Instant::now() + OBSERVE, || {
        lifecycle.snapshot().terminate_calls >= 1
    });
    let before_resume = lifecycle.snapshot();
    let retry_authority = wait_for_retention_authority(&retention);

    task.resume();
    let accepted_after_running = wait_until(Instant::now() + OBSERVE, || {
        lifecycle.snapshot().terminate_enqueued == 1
    });
    let actual_exit = wait_until(Instant::now() + CLEANUP, || {
        lifecycle.snapshot().actual_exit.is_some()
    });
    if !actual_exit {
        cleanup_child(&authority);
    }
    let final_snapshot = lifecycle.snapshot();
    let worker_finished = task.join_finished();

    assert!(paused);
    assert!(retained);
    assert!(attempted_prepared);
    assert_eq!(before_resume.state, ChildLifecycleState::Prepared);
    assert_eq!(before_resume.terminate_enqueued, 0);
    assert_eq!(retry_authority, Some(true));
    assert!(accepted_after_running);
    assert!(actual_exit);
    assert_eq!(final_snapshot.terminate_enqueued, 1);
    assert_eq!(final_snapshot.terminate_commands, 1);
    assert_eq!(final_snapshot.native_reaps, 1);
    assert!(worker_finished);
    Ok(())
}

#[test]
fn full_queue_attempt_without_accepted_terminate_preserves_retry_authority(
) -> Result<(), Box<dyn Error>> {
    let registry = LaunchRetentionRegistry::try_new()?;
    let retention = registry_probe(&registry);
    let fixture = spawn_fixture(
        fixture_spec(Some(FixtureSpawnFault::AfterBindBeforeBridge)),
        Arc::clone(&registry),
    );
    let authority = authority(&fixture)?;
    let lifecycle = lifecycle_probe(&authority);
    let running = wait_until(Instant::now() + OBSERVE, || {
        lifecycle.snapshot().state == ChildLifecycleState::Running
    });
    let pause = lifecycle.pause_before_next_poll();
    let paused = pause.wait_until_paused(Instant::now() + OBSERVE);
    let barrier = queue_barrier(&authority)?;
    let retained = fixture.finish(Arc::clone(&registry)).is_err();
    let attempted_full = wait_until(Instant::now() + OBSERVE, || {
        lifecycle.snapshot().terminate_calls >= 1
    });
    let before_resume = lifecycle.snapshot();
    let retry_authority = wait_for_retention_authority(&retention);

    drop(pause);
    let barrier_consumed = barrier.wait_until_consumed(Instant::now() + OBSERVE);
    let accepted_after_capacity = wait_until(Instant::now() + OBSERVE, || {
        lifecycle.snapshot().terminate_enqueued == 1
    });
    let actual_exit = wait_until(Instant::now() + CLEANUP, || {
        lifecycle.snapshot().actual_exit.is_some()
    });
    if !actual_exit {
        cleanup_child(&authority);
    }
    let final_snapshot = lifecycle.snapshot();

    assert!(running);
    assert!(paused);
    assert!(retained);
    assert!(attempted_full);
    assert_eq!(before_resume.terminate_enqueued, 0);
    assert_eq!(retry_authority, Some(true));
    assert!(barrier_consumed);
    assert_eq!(final_snapshot.barrier_commands, 1);
    assert!(accepted_after_capacity);
    assert!(actual_exit);
    assert_eq!(final_snapshot.terminate_enqueued, 1);
    assert_eq!(final_snapshot.terminate_commands, 1);
    assert_eq!(final_snapshot.native_reaps, 1);
    Ok(())
}

#[test]
fn accepted_terminate_timeout_never_restores_or_duplicates_authority() -> Result<(), Box<dyn Error>>
{
    let registry = LaunchRetentionRegistry::try_new()?;
    let retention = registry_probe(&registry);
    let fixture = spawn_fixture(
        fixture_spec(Some(FixtureSpawnFault::AfterBindBeforeBridge)),
        Arc::clone(&registry),
    );
    let authority = authority(&fixture)?;
    let lifecycle = lifecycle_probe(&authority);
    let running = wait_until(Instant::now() + OBSERVE, || {
        lifecycle.snapshot().state == ChildLifecycleState::Running
    });
    let pause = lifecycle.pause_before_next_poll();
    let paused = pause.wait_until_paused(Instant::now() + OBSERVE);
    let retained = fixture.finish(Arc::clone(&registry)).is_err();
    let accepted = wait_until(Instant::now() + OBSERVE, || {
        lifecycle.snapshot().terminate_enqueued == 1
    });
    let supervisor_returned = wait_until(Instant::now() + OBSERVE, || {
        retention.snapshot().managed_running >= 2
    });
    let timed_out = lifecycle.snapshot();
    let retry_authority = wait_for_retention_authority(&retention);

    drop(pause);
    let actual_exit = wait_until(Instant::now() + CLEANUP, || {
        lifecycle.snapshot().actual_exit.is_some()
    });
    if !actual_exit {
        cleanup_child(&authority);
    }
    let final_snapshot = lifecycle.snapshot();

    assert!(running);
    assert!(paused);
    assert!(retained);
    assert!(accepted);
    assert!(supervisor_returned);
    assert_eq!(retry_authority, Some(false));
    assert_eq!(timed_out.terminate_calls, 1);
    assert_eq!(timed_out.terminate_enqueued, 1);
    assert_eq!(timed_out.terminate_commands, 0);
    assert!(timed_out.actual_exit.is_none());
    assert_eq!(final_snapshot.terminate_calls, 1);
    assert_eq!(final_snapshot.terminate_enqueued, 1);
    assert_eq!(final_snapshot.terminate_commands, 1);
    assert_eq!(final_snapshot.native_reaps, 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_actor_never_blocks_spawn_resource_take_or_execution_drop(
) -> Result<(), Box<dyn Error>> {
    let setup_registry = LaunchRetentionRegistry::try_new()?;
    let mut setup_task = spawn_fixture_paused(
        fixture_spec(None),
        Arc::clone(&setup_registry),
        ActorPausePoint::BeforePoll,
    )?;
    let setup_paused = setup_task.wait_until_paused(Instant::now() + OBSERVE);
    let setup_custody = setup_task.custody_probe();
    let before_resume = setup_task.result_until(Instant::now() + OBSERVE);
    let setup_returned_while_paused = before_resume.is_some();
    let setup_release_before_reap = setup_custody.releases();
    setup_task.resume();
    let setup_fixture = match before_resume {
        Some(fixture) => fixture,
        None => setup_task
            .result_until(Instant::now() + CLEANUP)
            .ok_or("spawn setup did not return after releasing actor")?,
    };
    let setup_authority = authority(&setup_fixture)?;
    let setup_lifecycle = lifecycle_probe(&setup_authority);
    let setup_execution = setup_fixture.result.ok();
    cleanup_child(&setup_authority);
    drop(setup_execution);
    let setup_reaped = wait_until(Instant::now() + CLEANUP, || {
        setup_lifecycle.snapshot().actual_exit.is_some()
    });
    let setup_worker_finished = setup_task.join_finished();
    let setup_released_once =
        wait_until(Instant::now() + CLEANUP, || setup_custody.releases() == 1);

    let drop_registry = LaunchRetentionRegistry::try_new()?;
    let (drop_fixture, drop_custody) =
        spawn_fixture_with_custody_witness(fixture_spec(None), Arc::clone(&drop_registry));
    let drop_authority = authority(&drop_fixture)?;
    let drop_lifecycle = lifecycle_probe(&drop_authority);
    let execution = drop_fixture
        .result
        .map_err(|_| "running fixture did not return an execution")?;
    let pause = drop_lifecycle.pause_before_next_poll();
    let drop_paused = pause.wait_until_paused(Instant::now() + OBSERVE);

    let termination_authority = drop_authority.clone();
    let (termination_tx, termination_rx) = mpsc::sync_channel(1);
    let termination = std::thread::spawn(move || {
        let result = termination_authority.terminate_local_cli(Instant::now() + CLEANUP);
        let _ = termination_tx.send(result);
    });
    let terminate_accepted = wait_until(Instant::now() + OBSERVE, || {
        drop_lifecycle.snapshot().terminate_enqueued == 1
    });
    let (drop_tx, drop_rx) = mpsc::sync_channel(1);
    let drop_runtime = tokio::runtime::Handle::current();
    let drop_worker = std::thread::spawn(move || {
        let _runtime_guard = drop_runtime.enter();
        drop(execution);
        let _ = drop_tx.send(());
    });
    let drop_returned_while_paused = drop_rx.recv_timeout(OBSERVE).is_ok();
    let held = drop_lifecycle.snapshot();
    let release_before_reap = drop_custody.releases();
    let child_live_before_resume = held.native_reaps == 0 && held.actual_exit.is_none();

    drop(pause);
    let termination_result = termination_rx
        .recv_timeout(CLEANUP)
        .ok()
        .and_then(Result::ok);
    let drop_completed = drop_returned_while_paused || drop_rx.recv_timeout(CLEANUP).is_ok();
    let termination_finished = wait_until(Instant::now() + OBSERVE, || termination.is_finished());
    if termination_finished {
        let _ = termination.join();
    }
    let drop_worker_finished = wait_until(Instant::now() + OBSERVE, || drop_worker.is_finished());
    if drop_worker_finished {
        let _ = drop_worker.join();
    }
    let drop_reaped = wait_until(Instant::now() + CLEANUP, || {
        drop_lifecycle.snapshot().actual_exit.is_some()
    });
    drop(drop_authority);
    drop(drop_fixture.slot);
    let resources_released = wait_until(Instant::now() + CLEANUP, || {
        let snapshot = drop_lifecycle.snapshot();
        snapshot.actor_finished
            && !snapshot.resources_present
            && snapshot.resource_handoffs_in_flight == 0
            && drop_custody.releases() == 1
    });
    let final_snapshot = drop_lifecycle.snapshot();

    assert!(setup_paused);
    assert!(setup_returned_while_paused);
    assert!(setup_reaped);
    assert!(setup_worker_finished);
    assert_eq!(setup_release_before_reap, 0);
    assert!(setup_released_once);
    assert_eq!(setup_custody.releases(), 1);
    assert!(drop_paused);
    assert!(terminate_accepted);
    assert!(drop_returned_while_paused);
    assert!(drop_completed);
    assert!(termination_finished);
    assert!(drop_worker_finished);
    assert_eq!(held.resource_retain_calls, 1);
    assert_eq!(release_before_reap, 0);
    assert!(child_live_before_resume);
    assert!(termination_result.is_some());
    assert!(drop_reaped);
    assert!(resources_released);
    assert_eq!(drop_custody.releases(), 1);
    assert_eq!(final_snapshot.terminate_enqueued, 1);
    assert_eq!(final_snapshot.terminate_commands, 1);
    assert_eq!(final_snapshot.native_reaps, 1);
    Ok(())
}
