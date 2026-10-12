//! Protected host side of the split-control Apple startup gate.

use crate::data::startup_gate::{StartupGateAccess, StartupGateRequest};
use crate::engine::container::gated_launch::{
    provider_call_deadline, revalidate_post_spawn, AppleProviderState, ChildLifecycleAuthority,
    DurableLaunchPlan, ExactInspection, GatedProviderAdapter, LaunchRetentionReason, NamePresence,
    ProviderLaunchInspection, ProviderState,
};
#[cfg(test)]
use crate::engine::error::EngineError;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(test)]
use std::sync::Arc;
use std::time::{Duration, Instant};

const MAX_HANDSHAKE_RECORD: u64 = 64 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(20);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadyRecord {
    version: u32,
    nonce: String,
    container_name: String,
    bindings: Vec<ReadyBinding>,
}

#[derive(Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ReadyBinding {
    id: String,
    manifest_id: String,
    access: StartupGateAccess,
    workspace_path: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReleasedRecord {
    version: u32,
    nonce: String,
}

#[derive(Serialize)]
struct LaunchReceipt<'a> {
    version: u32,
    nonce: &'a str,
    container_name: &'a str,
    runtime_id: &'a str,
}

pub(super) struct ProtectedGateRelease {
    #[allow(dead_code)]
    pub inspection: ProviderLaunchInspection,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::engine::container) enum ExactCleanupPhase {
    NeedsCleanup,
    AwaitingAbsence {
        removal_authority: ProviderLaunchInspection,
    },
    Complete,
}

pub(super) type ExactCleanupError = Box<(LaunchRetentionReason, Option<ProviderLaunchInspection>)>;

fn cleanup_error(
    reason: LaunchRetentionReason,
    inspection: Option<ProviderLaunchInspection>,
) -> ExactCleanupError {
    Box::new((reason, inspection))
}

pub(super) fn await_protected_release(
    plan: &DurableLaunchPlan,
    adapter: &dyn GatedProviderAdapter,
    cancelled: &AtomicBool,
    enclosing: Instant,
    #[cfg(test)] boundary: Option<&dyn OrchestratedAppleTestBoundary>,
) -> Result<ProtectedGateRelease, LaunchRetentionReason> {
    #[cfg(test)]
    if let Some(boundary) = boundary {
        boundary.prepare_simulated_guest(plan)?;
        if boundary.pause_before_ready() {
            boundary.observe_cancel_point(TestCancellationPoint::BeforeReady);
        }
    }

    let ready = loop {
        if cancelled.load(Ordering::Acquire) {
            return Err(LaunchRetentionReason::ChildStateUnknown);
        }
        if Instant::now() >= enclosing {
            return Err(LaunchRetentionReason::InspectionUnavailable);
        }
        match plan
            .controls
            .read_guest_record("ready.json", MAX_HANDSHAKE_RECORD)
        {
            Ok(Some(bytes)) => break parse_ready(plan, &bytes)?,
            Ok(None) => std::thread::sleep(POLL_INTERVAL),
            Err(_) => return Err(LaunchRetentionReason::PostSpawnControlChanged),
        }
    };

    let inspection = match adapter.inspect_launch(&plan.key, provider_call_deadline(enclosing)) {
        ExactInspection::Matching(inspection) => inspection,
        ExactInspection::Absent(_) | ExactInspection::ForeignOrAmbiguous => {
            return Err(LaunchRetentionReason::SpawnResultUnknown);
        }
        ExactInspection::Unavailable => {
            return Err(LaunchRetentionReason::InspectionUnavailable);
        }
    };

    revalidate_post_spawn(plan).map_err(|_| LaunchRetentionReason::PostSpawnControlChanged)?;
    let confirmed = match adapter.inspect_launch(&plan.key, provider_call_deadline(enclosing)) {
        ExactInspection::Matching(confirmed) if confirmed == inspection => confirmed,
        ExactInspection::Matching(_)
        | ExactInspection::Absent(_)
        | ExactInspection::ForeignOrAmbiguous => {
            return Err(LaunchRetentionReason::SpawnResultUnknown);
        }
        ExactInspection::Unavailable => {
            return Err(LaunchRetentionReason::InspectionUnavailable);
        }
    };
    if cancelled.load(Ordering::Acquire) || Instant::now() >= enclosing {
        return Err(LaunchRetentionReason::ChildStateUnknown);
    }
    revalidate_post_spawn(plan).map_err(|_| LaunchRetentionReason::PostSpawnControlChanged)?;
    #[cfg(test)]
    if let Some(boundary) = boundary {
        boundary.observe_protected_launch_identity(&confirmed);
    }

    let receipt = serde_json::to_vec(&LaunchReceipt {
        version: 1,
        nonce: &ready.nonce,
        container_name: plan.key.container_name.as_str(),
        runtime_id: &confirmed.runtime_id,
    })
    .map_err(|_| LaunchRetentionReason::PostSpawnControlChanged)?;
    let pinned_receipt = plan
        .controls
        .host_parent
        .publish_launch_receipt(&receipt)
        .map_err(|_| LaunchRetentionReason::PostSpawnControlChanged)?;
    if !plan
        .controls
        .host_parent
        .verify_launch_receipt(&pinned_receipt)
    {
        return Err(LaunchRetentionReason::PostSpawnControlChanged);
    }
    revalidate_post_spawn(plan).map_err(|_| LaunchRetentionReason::PostSpawnControlChanged)?;

    #[cfg(test)]
    if let Some(boundary) = boundary {
        boundary.observe_protected_ready();
        if boundary.pause_after_ready() {
            boundary.observe_cancel_point(TestCancellationPoint::AfterReadyBeforeRelease);
            while !cancelled.load(Ordering::Acquire) && Instant::now() < enclosing {
                std::thread::sleep(POLL_INTERVAL);
            }
            return Err(LaunchRetentionReason::ChildStateUnknown);
        }
    }

    loop {
        if cancelled.load(Ordering::Acquire) {
            return Err(LaunchRetentionReason::ChildStateUnknown);
        }
        if Instant::now() >= enclosing {
            return Err(LaunchRetentionReason::InspectionUnavailable);
        }
        match plan.controls.read_guest_record(".released", 4096) {
            Ok(Some(bytes)) => {
                let released: ReleasedRecord = serde_json::from_slice(&bytes)
                    .map_err(|_| LaunchRetentionReason::PostSpawnControlChanged)?;
                if released.version != 1 || released.nonce != ready.nonce {
                    return Err(LaunchRetentionReason::PostSpawnControlChanged);
                }
                break;
            }
            Ok(None) => std::thread::sleep(POLL_INTERVAL),
            Err(_) => return Err(LaunchRetentionReason::PostSpawnControlChanged),
        }
    }

    #[cfg(test)]
    if let Some(boundary) = boundary {
        boundary.observe_accepted_release();
    }
    Ok(ProtectedGateRelease {
        inspection: confirmed,
    })
}

fn parse_ready(
    plan: &DurableLaunchPlan,
    bytes: &[u8],
) -> Result<ReadyRecord, LaunchRetentionReason> {
    let ready: ReadyRecord = serde_json::from_slice(bytes)
        .map_err(|_| LaunchRetentionReason::PostSpawnControlChanged)?;
    if ready.version != 1
        || ready.container_name != plan.key.container_name.as_str()
        || !valid_lower_hex_32(&ready.nonce)
        || ready.bindings != expected_bindings(&plan.controls.request.request)
    {
        return Err(LaunchRetentionReason::PostSpawnControlChanged);
    }
    Ok(ready)
}

fn expected_bindings(request: &StartupGateRequest) -> Vec<ReadyBinding> {
    request
        .bindings
        .iter()
        .map(|binding| ReadyBinding {
            id: binding.id.clone(),
            manifest_id: binding.manifest_id.clone(),
            access: binding.access,
            workspace_path: binding.workspace_path.clone(),
        })
        .collect()
}

fn valid_lower_hex_32(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(super) fn cleanup_exact_launch(
    plan: &DurableLaunchPlan,
    adapter: &dyn GatedProviderAdapter,
    lifecycle: &ChildLifecycleAuthority,
    phase: &mut ExactCleanupPhase,
    enclosing: Instant,
    #[cfg(test)] boundary: Option<&dyn OrchestratedAppleTestBoundary>,
) -> Result<(), ExactCleanupError> {
    if matches!(phase, ExactCleanupPhase::Complete) {
        return Ok(());
    }
    if let ExactCleanupPhase::AwaitingAbsence { removal_authority } = phase {
        let removal_authority = removal_authority.clone();
        return retry_authorized_removal(plan, adapter, phase, enclosing, &removal_authority);
    }
    #[cfg(test)]
    if let Some(boundary) = boundary {
        boundary.begin_cleanup();
    }
    let first = match adapter.inspect_launch(&plan.key, provider_call_deadline(enclosing)) {
        ExactInspection::Matching(inspection) => inspection,
        ExactInspection::Absent(_) | ExactInspection::ForeignOrAmbiguous => {
            return Err(cleanup_error(
                LaunchRetentionReason::SpawnResultUnknown,
                None,
            ));
        }
        ExactInspection::Unavailable => {
            return Err(cleanup_error(
                LaunchRetentionReason::InspectionUnavailable,
                None,
            ));
        }
    };
    adapter
        .stop_inspected(&first, provider_call_deadline(enclosing))
        .map_err(|_| {
            cleanup_error(
                LaunchRetentionReason::InspectionUnavailable,
                Some(first.clone()),
            )
        })?;
    #[cfg(test)]
    if let Some(boundary) = boundary {
        boundary.observe_reap();
    }
    if !matches!(lifecycle.wait_actual_until(Instant::now()), Ok(Some(_)))
        && lifecycle.terminate_local_cli(enclosing).is_err()
    {
        return Err(cleanup_error(
            LaunchRetentionReason::ChildStateUnknown,
            Some(first),
        ));
    }
    #[cfg(test)]
    if boundary.is_some_and(|boundary| !boundary.cleanup_reap_proven()) {
        return Err(cleanup_error(
            LaunchRetentionReason::ChildStateUnknown,
            Some(first),
        ));
    }
    let stopped = match adapter.inspect_launch(&plan.key, provider_call_deadline(enclosing)) {
        ExactInspection::Matching(inspection)
            if same_launch_identity(&inspection, &first)
                && inspection.state == ProviderState::Apple(AppleProviderState::Stopped) =>
        {
            inspection
        }
        ExactInspection::Matching(_) => {
            return Err(cleanup_error(
                LaunchRetentionReason::SpawnResultUnknown,
                Some(first),
            ));
        }
        ExactInspection::Absent(_) | ExactInspection::ForeignOrAmbiguous => {
            return Err(cleanup_error(
                LaunchRetentionReason::SpawnResultUnknown,
                Some(first),
            ));
        }
        ExactInspection::Unavailable => {
            return Err(cleanup_error(
                LaunchRetentionReason::InspectionUnavailable,
                Some(first),
            ));
        }
    };
    *phase = ExactCleanupPhase::AwaitingAbsence {
        removal_authority: stopped.clone(),
    };
    adapter
        .remove_inspected(&stopped, provider_call_deadline(enclosing))
        .map_err(|_| {
            cleanup_error(
                LaunchRetentionReason::InspectionUnavailable,
                Some(stopped.clone()),
            )
        })?;
    prove_exact_absence(plan, adapter, phase, enclosing, Some(stopped))
}

fn retry_authorized_removal(
    plan: &DurableLaunchPlan,
    adapter: &dyn GatedProviderAdapter,
    phase: &mut ExactCleanupPhase,
    enclosing: Instant,
    removal_authority: &ProviderLaunchInspection,
) -> Result<(), ExactCleanupError> {
    match adapter.inspect_name_absence(&plan.key.container_name, provider_call_deadline(enclosing))
    {
        Ok(absence)
            if absence.provider == plan.key.provider
                && absence.exact_name == plan.key.container_name =>
        {
            *phase = ExactCleanupPhase::Complete;
            return Ok(());
        }
        Ok(_) | Err(NamePresence::Ambiguous | NamePresence::Unavailable) => {
            return Err(cleanup_error(
                LaunchRetentionReason::InspectionUnavailable,
                Some(removal_authority.clone()),
            ));
        }
        Err(NamePresence::Present) => {}
    }

    let current = match adapter.inspect_launch(&plan.key, provider_call_deadline(enclosing)) {
        ExactInspection::Matching(current)
            if same_launch_identity(&current, removal_authority)
                && current.state == ProviderState::Apple(AppleProviderState::Stopped) =>
        {
            current
        }
        ExactInspection::Matching(current) => {
            return Err(cleanup_error(
                LaunchRetentionReason::SpawnResultUnknown,
                Some(current),
            ));
        }
        ExactInspection::Absent(_) | ExactInspection::ForeignOrAmbiguous => {
            return Err(cleanup_error(
                LaunchRetentionReason::SpawnResultUnknown,
                Some(removal_authority.clone()),
            ));
        }
        ExactInspection::Unavailable => {
            return Err(cleanup_error(
                LaunchRetentionReason::InspectionUnavailable,
                Some(removal_authority.clone()),
            ));
        }
    };
    adapter
        .remove_inspected(&current, provider_call_deadline(enclosing))
        .map_err(|_| {
            cleanup_error(
                LaunchRetentionReason::InspectionUnavailable,
                Some(current.clone()),
            )
        })?;
    prove_exact_absence(plan, adapter, phase, enclosing, Some(current))
}

fn same_launch_identity(
    current: &ProviderLaunchInspection,
    authorized: &ProviderLaunchInspection,
) -> bool {
    current.provider == authorized.provider
        && current.runtime_id == authorized.runtime_id
        && current.exact_name == authorized.exact_name
        && current.token_digest == authorized.token_digest
        && current.immutable_image_id == authorized.immutable_image_id
        && current.created_at == authorized.created_at
}

fn prove_exact_absence(
    plan: &DurableLaunchPlan,
    adapter: &dyn GatedProviderAdapter,
    phase: &mut ExactCleanupPhase,
    enclosing: Instant,
    last_inspection: Option<ProviderLaunchInspection>,
) -> Result<(), ExactCleanupError> {
    match adapter.inspect_name_absence(&plan.key.container_name, provider_call_deadline(enclosing))
    {
        Ok(absence)
            if absence.provider == plan.key.provider
                && absence.exact_name == plan.key.container_name =>
        {
            *phase = ExactCleanupPhase::Complete;
            Ok(())
        }
        Ok(_) | Err(_) => Err(cleanup_error(
            LaunchRetentionReason::InspectionUnavailable,
            last_inspection,
        )),
    }
}

#[cfg(test)]
#[derive(Clone, Copy)]
pub(crate) enum TestCancellationPoint {
    BeforeReady,
    AfterReadyBeforeRelease,
}

#[cfg(test)]
#[derive(Default)]
pub(crate) struct TestOwnershipObservation {
    pub exact_plan: bool,
    pub child: bool,
    pub container: bool,
    pub controls: bool,
    pub staged_files: bool,
    pub registry_ticket: bool,
    pub quiescence_proven: bool,
}

#[cfg(test)]
pub(crate) trait OrchestratedAppleTestBoundary: Send + Sync {
    fn retention_available(&self) -> bool;
    fn image_inspection(&self) -> Result<Vec<u8>, EngineError>;
    fn provider(&self) -> Arc<dyn GatedProviderAdapter>;
    fn executable(&self) -> &'static str;
    fn prepare_simulated_guest(
        &self,
        plan: &DurableLaunchPlan,
    ) -> Result<(), LaunchRetentionReason>;
    fn pause_before_ready(&self) -> bool;
    fn pause_after_ready(&self) -> bool;
    #[allow(clippy::too_many_arguments)]
    fn observe_spawn(
        &self,
        argv: Vec<String>,
        container_name: String,
        labels: Vec<String>,
        writable_mounts: Vec<(String, bool)>,
        registry_arc_reached_spawn: bool,
        staged_request: Vec<u8>,
        staged_manifests: std::collections::BTreeMap<String, Vec<u8>>,
    );
    fn observe_child_start(&self);
    fn observe_protected_launch_identity(&self, inspection: &ProviderLaunchInspection);
    fn observe_protected_ready(&self);
    fn observe_accepted_release(&self);
    fn observe_provider_entry(&self, before_release: bool);
    fn observe_cancel_point(&self, point: TestCancellationPoint);
    fn begin_cleanup(&self);
    fn observe_reap(&self);
    fn cleanup_reap_proven(&self) -> bool;
    fn observe_custody(&self, ownership: TestOwnershipObservation, legacy: bool);
}
