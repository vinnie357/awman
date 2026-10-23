use super::{
    exercise_public_path_with_native_and_cancel_driver_hook, AppleCommandScenario,
    CancelDriverFailure, CancellationPoint,
};

#[test]
fn cancellation_boundary_driver_failure_is_not_replaced_by_a_report() {
    let mut injected = false;
    let result = exercise_public_path_with_native_and_cancel_driver_hook(
        AppleCommandScenario::Cancel(CancellationPoint::BeforeReady),
        None,
        |failure| {
            assert!(
                failure.is_none(),
                "unexpected real driver failure: {failure:?}"
            );
            *failure = Some(CancelDriverFailure::BoundaryNotObserved(
                CancellationPoint::BeforeReady,
            ));
            injected = true;
        },
    );
    assert!(injected, "fixture did not reach the driver outcome hook");
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("cancellation driver failure was replaced by a report"),
    };
    assert!(
        matches!(
            error.downcast_ref::<CancelDriverFailure>(),
            Some(CancelDriverFailure::BoundaryNotObserved(
                CancellationPoint::BeforeReady
            ))
        ),
        "expected the typed cancellation boundary failure, got {error}"
    );
}
