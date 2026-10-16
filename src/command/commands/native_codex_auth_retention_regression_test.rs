//! Regression: a plain command error cannot own the only unresolved preparation lease.
use super::native_codex_auth_entry_test::{command, frontend};
use super::Command;
use crate::command::error::CommandError;
use crate::engine::agent::native_codex_auth_preparation_test::{Fixture, TestResult};
use crate::engine::container::gated_launch::LaunchIdentityError;
use crate::engine::error::EngineError;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::Ordering;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn command_error_drops_no_unresolved_plan_custody_even_after_registry_shutdown() -> TestResult
{
    let f = Fixture::new(&[])?;
    f.observer.seed_existing_plan.store(true, Ordering::SeqCst);
    let (front, _output) = frontend(&f);
    let result = command(&f).run_with_frontend(front).await;
    let reason = match result {
        Err(CommandError::Engine(EngineError::Container(reason))) => reason,
        Err(other) => panic!("expected existing-plan Container error, received: {other:?}"),
        Ok(_) => panic!("expected existing-plan recovery error, received command success"),
    };
    assert_eq!(
        reason,
        LaunchIdentityError::ExistingPlanRecoveryRequired.to_string()
    );
    drop(reason); // No caller error/opaque result remains available to hold the sole lease.
    let (conflict, activation) = {
        let facts = f.observer.facts.lock().expect("facts");
        assert_eq!(
            facts.seeded_plan_conflicts.len(),
            1,
            "must seed the actual plan-store parent"
        );
        assert!(facts.argv.is_empty());
        for event in [
            "child-start",
            "provider-stop",
            "exact-remove",
            "cleanup-published",
            "resources-released",
        ] {
            assert!(
                !facts.events.contains(&event),
                "unknown existing plan grants no lifecycle/cleanup authority"
            );
        }
        (
            facts.seeded_plan_conflicts[0].clone(),
            facts
                .material
                .first()
                .expect("activated original owner")
                .1
                .clone(),
        )
    };
    assert_eq!(fs::read(&conflict)?, b"unresolved-auth-plan-fixture\n");
    assert_eq!(fs::metadata(&conflict)?.permissions().mode() & 0o777, 0o600);
    f.observer.assert_locked();
    f.observer.assert_unpublished();
    let ticket = f.assert_and_poll_unresolved();
    assert_eq!(f.observer.activation(), activation);
    assert_eq!(fs::read(&conflict)?, b"unresolved-auth-plan-fixture\n");
    let preserved = f.keep_expected_unresolved(ticket);
    preserved.assert_shutdown_retains(ticket);
    assert_eq!(preserved.observer.activation(), activation);
    assert_eq!(fs::read(&conflict)?, b"unresolved-auth-plan-fixture\n");
    assert!(preserved
        .observer
        .facts
        .lock()
        .expect("facts")
        .events
        .iter()
        .all(|event| ![
            "child-start",
            "provider-stop",
            "exact-remove",
            "cleanup-published",
            "resources-released"
        ]
        .contains(event)));
    // No mem::forget or invented cleanup: existing Shared keeps actual unresolved custody.
    // The persistent synthetic fixture is deliberately retained for this one case.
    Ok(())
}
