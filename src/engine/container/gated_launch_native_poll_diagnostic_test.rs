// Temporary diagnostic composition only. The included frozen fixture is pinned
// separately and is neither edited nor executed by the focused diagnostic run.
include!("gated_launch_p2c_test.rs");

const NATIVE_POLL_DIAGNOSTIC_DEADLINE: Duration = Duration::from_secs(3);

fn lifecycle_state_label(state: &ChildLifecycleState) -> &'static str {
    match state {
        ChildLifecycleState::Prepared => "prepared",
        ChildLifecycleState::Running => "running",
        ChildLifecycleState::Failed => "failed",
        ChildLifecycleState::Exited(_) => "exited",
    }
}

#[cfg(unix)]
#[test]
fn diagnostic_bind_fault_records_bounded_last_unbound_native_poll_observation(
) -> Result<(), Box<dyn Error>> {
    for index in 0..3 {
        for disconnected in [true, false] {
            let Invocation {
                local,
                _controls,
                plan,
                adapter,
                registry,
                spawned,
            } = invoke(InvokeOptions {
                mode: mode(index),
                code: 0,
                output: "bind-fault-diagnostic".into(),
                grace_timeout: Duration::from_secs(2),
                executable: SHELL,
                fault: Some(if disconnected {
                    FixtureSpawnFault::BindDisconnected
                } else {
                    FixtureSpawnFault::BindFull
                }),
                replies: vec![],
            })?;
            let (source, owned) = after_start(
                spawned
                    .result
                    .err()
                    .ok_or("bind diagnostic fault unexpectedly succeeded")?,
            )?;
            assert!(matches!(
                source,
                EngineError::Container(ref message) if message == "create CLI state unknown"
            ));
            assert!(matches!(&owned, RetainedExecution::OwnedUnbound { .. }));

            let probe = registry_probe(&registry);
            let ticket = registry.retain(RetainedAgentLaunch {
                plan,
                execution: Some(owned),
                last_inspection: None,
                reason: LaunchRetentionReason::ChildStateUnknown,
            });
            assert!(probe.contains(ticket));
            let initial = probe.snapshot();
            local.release()?;

            let deadline = Instant::now() + NATIVE_POLL_DIAGNOSTIC_DEADLINE;
            let final_snapshot = loop {
                let snapshot = probe.snapshot();
                if snapshot.unbound_reaped > 0 || Instant::now() >= deadline {
                    break snapshot;
                }
                std::thread::sleep(Duration::from_millis(10));
            };
            eprintln!(
                "AW_NATIVE_POLL bind kind={:?} disconnected={} initial_retained={} initial_unreaped={} attempts={} pending={} errors={} reaped={} pid={:?} errno={:?} resources_present={} final_retained={} final_unreaped={} worker_finished={}",
                kind(index),
                disconnected,
                initial.retained,
                initial.unreaped,
                final_snapshot.unbound_attempts,
                final_snapshot.unbound_pending,
                final_snapshot.unbound_errors,
                final_snapshot.unbound_reaped,
                final_snapshot.last_pid,
                final_snapshot.last_errno,
                final_snapshot.last_resources_present,
                final_snapshot.retained,
                final_snapshot.unreaped,
                final_snapshot.worker_finished,
            );
            assert!(
                final_snapshot.unbound_attempts > 0,
                "bounded diagnostic observed no unbound native poll"
            );
            assert_eq!(
                final_snapshot.unbound_attempts,
                final_snapshot.unbound_pending
                    + final_snapshot.unbound_errors
                    + final_snapshot.unbound_reaped
            );
            assert!(final_snapshot.last_resources_present);
            assert!(local.starts()? <= 1);
            assert_eq!(adapter.destructive_calls(), 0);
        }
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn diagnostic_post_bind_bridge_fault_records_bounded_lifecycle_native_poll_observation(
) -> Result<(), Box<dyn Error>> {
    for index in 0..3 {
        let Invocation {
            local,
            _controls,
            plan,
            adapter,
            registry,
            spawned,
        } = invoke(InvokeOptions {
            mode: mode(index),
            code: 0,
            output: "post-bind-fault-diagnostic".into(),
            grace_timeout: Duration::from_secs(2),
            executable: SHELL,
            fault: Some(FixtureSpawnFault::AfterBindBeforeBridge),
            replies: vec![LaunchReply::Matching],
        })?;
        let (source, owned) = after_start(
            spawned
                .result
                .err()
                .ok_or("post-bind diagnostic fault unexpectedly succeeded")?,
        )?;
        assert!(matches!(
            source,
            EngineError::Container(ref message) if message == "create CLI bridge setup failed"
        ));
        let authority = match &owned {
            RetainedExecution::Managed {
                execution: None,
                lifecycle,
            } => lifecycle.clone(),
            _ => return Err("post-bind diagnostic did not retain managed custody".into()),
        };
        let lifecycle = lifecycle_probe(&authority);
        let retained = registry_probe(&registry);
        let ticket = registry.retain(RetainedAgentLaunch {
            plan,
            execution: Some(owned),
            last_inspection: None,
            reason: LaunchRetentionReason::SpawnResultUnknown,
        });
        assert!(retained.contains(ticket));
        let initial_lifecycle = lifecycle.snapshot();
        let initial_retention = retained.snapshot();
        local.release()?;

        let deadline = Instant::now() + NATIVE_POLL_DIAGNOSTIC_DEADLINE;
        let (final_lifecycle, final_retention) = loop {
            let lifecycle_snapshot = lifecycle.snapshot();
            let retention_snapshot = retained.snapshot();
            if lifecycle_snapshot.native_poll_reaped > 0
                || matches!(&lifecycle_snapshot.state, ChildLifecycleState::Exited(_))
                || Instant::now() >= deadline
            {
                break (lifecycle_snapshot, retention_snapshot);
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        eprintln!(
            "AW_NATIVE_POLL post_bind kind={:?} initial_state={} initial_attempts={} initial_retained={} initial_unreaped={} state={} attempts={} pending={} errors={} reaped={} pid={:?} errno={:?} resources_present={} native_reaps={} owns_unreaped_child={} managed_prepared={} managed_running={} managed_failed={} managed_exited={} registry_last_unbound_pid={:?} registry_last_unbound_errno={:?} registry_last_unbound_resources={} final_retained={} final_unreaped={} worker_finished={}",
            kind(index),
            lifecycle_state_label(&initial_lifecycle.state),
            initial_lifecycle.native_poll_attempts,
            initial_retention.retained,
            initial_retention.unreaped,
            lifecycle_state_label(&final_lifecycle.state),
            final_lifecycle.native_poll_attempts,
            final_lifecycle.native_poll_pending,
            final_lifecycle.native_poll_errors,
            final_lifecycle.native_poll_reaped,
            final_lifecycle.bound_pid,
            final_lifecycle.last_poll_errno,
            final_lifecycle.resources_present,
            final_lifecycle.native_reaps,
            final_lifecycle.owns_unreaped_child,
            final_retention.managed_prepared,
            final_retention.managed_running,
            final_retention.managed_failed,
            final_retention.managed_exited,
            final_retention.last_pid,
            final_retention.last_errno,
            final_retention.last_resources_present,
            final_retention.retained,
            final_retention.unreaped,
            final_retention.worker_finished,
        );
        assert!(
            final_lifecycle.native_poll_attempts > 0,
            "bounded diagnostic observed no managed native poll"
        );
        assert_eq!(
            final_lifecycle.native_poll_attempts,
            final_lifecycle.native_poll_pending
                + final_lifecycle.native_poll_errors
                + final_lifecycle.native_poll_reaped
        );
        assert!(final_lifecycle.bound_pid.is_some());
        assert!(final_lifecycle.resources_present);
        assert_eq!(final_retention.last_pid, None);
        assert_eq!(final_retention.last_errno, None);
        assert!(!final_retention.last_resources_present);
        assert!(local.starts()? <= 1);
        assert_eq!(adapter.destructive_calls(), 0);
    }
    Ok(())
}
