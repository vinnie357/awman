use super::orchestrated_apple_command_acceptance_test::{
    exercise, AppleCommandReport, AppleCommandScenario, CleanupFault, CleanupStep, ProductionCall,
    TestDisposition,
};

fn assert_public_owner_path(report: &AppleCommandReport) {
    assert!(report.production_calls.starts_with(&[
        ProductionCall::AgentEngineResolve,
        ProductionCall::ContainerOptionsResolve,
        ProductionCall::RuntimeBuild,
        ProductionCall::RunWithFrontend,
    ]));
    assert_eq!(report.spawn_attempts, 1);
    assert_eq!(report.child_starts, 1);
    assert_eq!(report.protected_ready_transitions, 1);
    assert_eq!(report.accepted_releases, 1);
    assert!(!report.second_spawn_attempted);
    assert!(!report.name_only_cleanup_attempted);
}

fn assert_whole_custody_retained(report: &AppleCommandReport) {
    assert_eq!(report.disposition, TestDisposition::Retained);
    assert!(report.ownership.exact_plan);
    assert!(report.ownership.child);
    assert!(report.ownership.container);
    assert!(report.ownership.controls);
    assert!(report.ownership.staged_files);
    assert!(report.ownership.registry_ticket);
    assert!(report.cleanup_retained_ticket.is_some());
    assert_eq!(report.cleanup_retained_ticket_present, Some(true));
    assert_eq!(report.cleanup_active_resource_loans, Some(1));
    assert_eq!(report.cleanup_resource_loan_releases, Some(0));
}

fn assert_cleanup_identity_is_original_launch_authority(report: &AppleCommandReport) {
    let planned = report
        .planned_launch_identity
        .as_ref()
        .expect("sealed launch plan identity was not observed");
    let protected = report
        .protected_launch_identity
        .as_ref()
        .expect("production-confirmed protected launch identity was not observed");
    let first = report
        .cleanup_inspection_identities
        .first()
        .expect("first cleanup inspection identity was not observed");
    assert_eq!(protected.provider, planned.provider);
    assert_eq!(protected.exact_name, planned.exact_name);
    assert_eq!(protected.token_digest, planned.token_digest);
    assert_eq!(protected.immutable_image_id, planned.immutable_image_id);
    assert!(protected.created_at >= planned.created_not_before);
    assert_eq!(
        report.protected_receipt_runtime_id.as_deref(),
        Some(protected.runtime_id.as_str())
    );
    assert_eq!(
        report.protected_receipt_container_name.as_deref(),
        Some(protected.exact_name.as_str())
    );
    assert_eq!(first, protected);
}

fn assert_changed_second_identity_is_retained(fault: CleanupFault) {
    let report = exercise(AppleCommandScenario::CleanupUncertain(fault));
    assert_public_owner_path(&report);
    assert_whole_custody_retained(&report);
    assert_cleanup_identity_is_original_launch_authority(&report);
    let [first, second] = report.cleanup_inspection_identities.as_slice() else {
        panic!("changed cleanup identity fixture did not publish exactly two inspections");
    };
    assert_eq!(first.provider, second.provider);
    assert_eq!(first.exact_name, second.exact_name);
    assert_eq!(first.token_digest, second.token_digest);
    assert_eq!(first.immutable_image_id, second.immutable_image_id);
    match fault {
        CleanupFault::PreRemoveRuntimeIdChanged => {
            assert_ne!(first.runtime_id, second.runtime_id);
            assert_eq!(first.created_at, second.created_at);
        }
        CleanupFault::PreRemoveCreatedAtChanged => {
            assert_eq!(first.runtime_id, second.runtime_id);
            assert_ne!(first.created_at, second.created_at);
        }
        _ => panic!("changed cleanup identity helper received an unrelated fault"),
    }
    assert_eq!(
        report.stop_identities.as_slice(),
        std::slice::from_ref(first)
    );
    assert!(report.remove_identities.is_empty());
    assert_eq!(
        report.cleanup_steps,
        vec![
            CleanupStep::Inspect,
            CleanupStep::Stop,
            CleanupStep::Reap,
            CleanupStep::Inspect,
        ]
    );
    assert_eq!(
        report
            .cleanup_steps
            .iter()
            .filter(|step| **step == CleanupStep::Remove)
            .count(),
        0
    );
}

#[test]
fn changed_runtime_id_after_stop_cannot_authorize_remove() {
    assert_changed_second_identity_is_retained(CleanupFault::PreRemoveRuntimeIdChanged);
}

#[test]
fn changed_creation_timestamp_after_stop_cannot_authorize_remove() {
    assert_changed_second_identity_is_retained(CleanupFault::PreRemoveCreatedAtChanged);
}

#[test]
fn unchanged_immutable_identity_allows_one_authorized_remove() {
    let report = exercise(AppleCommandScenario::Qualified);
    assert_public_owner_path(&report);
    assert_eq!(report.disposition, TestDisposition::Completed);
    assert_cleanup_identity_is_original_launch_authority(&report);
    let [first, stopped] = report.cleanup_inspection_identities.as_slice() else {
        panic!("stable cleanup identity fixture did not publish exactly two inspections");
    };
    assert_eq!(first, stopped);
    assert_eq!(
        report.stop_identities.as_slice(),
        std::slice::from_ref(first)
    );
    assert_eq!(
        report.remove_identities.as_slice(),
        std::slice::from_ref(first)
    );
    assert_eq!(
        report.cleanup_steps,
        vec![
            CleanupStep::Inspect,
            CleanupStep::Stop,
            CleanupStep::Reap,
            CleanupStep::Inspect,
            CleanupStep::Remove,
            CleanupStep::ProveAbsent,
        ]
    );
    assert_eq!(
        report
            .cleanup_steps
            .iter()
            .filter(|step| **step == CleanupStep::Stop)
            .count(),
        1
    );
    assert_eq!(
        report
            .cleanup_steps
            .iter()
            .filter(|step| **step == CleanupStep::Remove)
            .count(),
        1
    );
    assert!(report.ownership.quiescence_proven);
}
