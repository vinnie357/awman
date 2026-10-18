use std::any::Any;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::child_lifecycle::{
    ChildLifecycleAuthority, ChildLifecycleState, ExecutionResourceLoan,
    NeverCreatedChildLifecycle, NeverCreatedRetentionFailure, PreparedChildLifecycle,
    RawCustodyCapability, RawCustodyOutcome, RetainedExecution, TerminateRequestOutcome,
};
use super::{
    DurableLaunchPlan, GatedProviderAdapter, PendingPublication, ProviderLaunchInspection,
};
use crate::engine::container::native_sdk_execution::NativeSdkExecution;
#[cfg(test)]
use crate::engine::container::orchestrated_apple::OrchestratedAppleTestBoundary;
use crate::engine::error::EngineError;

pub(crate) const REGISTRY_SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
const SUPERVISOR_POLL_INTERVAL: Duration = Duration::from_millis(20);
const SUPERVISOR_ACTIVE_RETRY_BOUND: Duration = Duration::from_secs(2);
const RETAINED_LOCAL_CLI_TERMINATE_BOUND: Duration = Duration::from_millis(50);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)]
pub(crate) enum LaunchRetentionReason {
    SpawnResultUnknown,
    ChildStateUnknown,
    InspectionUnavailable,
    PostSpawnControlChanged,
    BridgeSetupFailed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ActiveGatedRetryOutcome {
    Resolved,
    Retained,
}

pub(crate) struct ActiveGateMonitor {
    cancelled: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<Result<(), LaunchRetentionReason>>>,
}

impl ActiveGateMonitor {
    pub(crate) fn new(
        cancelled: Arc<AtomicBool>,
        worker: Option<std::thread::JoinHandle<Result<(), LaunchRetentionReason>>>,
    ) -> Self {
        Self { cancelled, worker }
    }

    pub(crate) fn wait(&mut self) -> Option<Result<(), LaunchRetentionReason>> {
        self.worker.take().map(|worker| {
            worker
                .join()
                .unwrap_or(Err(LaunchRetentionReason::ChildStateUnknown))
        })
    }

    fn cancel_and_wait_until(&mut self, deadline: Instant) -> bool {
        self.cancelled.store(true, Ordering::Release);
        let Some(worker) = self.worker.as_ref() else {
            return false;
        };
        while !worker.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        if !worker.is_finished() {
            return false;
        }
        let Some(worker) = self.worker.take() else {
            return false;
        };
        let _ = worker.join();
        true
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)] // Frozen Packet 1 surface; no longer embedded in SpawnStageError.
pub(crate) enum NotCreatedProof {
    CliNeverStarted,
}

pub(crate) struct RetainedAgentLaunch {
    pub(crate) plan: DurableLaunchPlan,
    pub(crate) execution: Option<RetainedExecution>,
    pub(crate) last_inspection: Option<ProviderLaunchInspection>,
    pub(crate) reason: LaunchRetentionReason,
}

impl fmt::Debug for RetainedAgentLaunch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RetainedAgentLaunch")
            .field("plan", &self.plan)
            .field("execution", &self.execution)
            .field("last_inspection", &self.last_inspection)
            .field("reason", &self.reason)
            .finish()
    }
}

use crate::engine::agent::native_codex_preparation::SdkCompletionReconcile;

enum RetainedRegistryEnvelope {
    Gated(Box<RetainedAgentLaunch>),
    PendingPublication {
        publication: Option<Box<PendingPublication>>,
        prepared: Option<PreparedChildLifecycle>,
        resources: Option<Box<dyn Any + Send>>,
        adapter: Arc<dyn GatedProviderAdapter>,
        #[cfg(test)]
        boundary: Option<Arc<dyn OrchestratedAppleTestBoundary>>,
    },
    ActiveGated(Box<ActiveGatedCustody>),
    // Original pre-spawn auth custody with an untrusted existing plan. This
    // envelope deliberately has no promotion, cleanup or discharge operation.
    UnresolvedAuthPreparation {
        _prepared: PreparedChildLifecycle,
        _resources: Box<dyn Any + Send>,
        _auth: Arc<crate::engine::agent::native_codex_preparation::AuthCustody>,
    },
    SdkCompletionUncertain(crate::engine::agent::native_codex_preparation::SdkCompletionCustody),
    UnresolvedMainPreparation {
        _prepared: PreparedChildLifecycle,
        _resources: Box<dyn Any + Send>,
        _main: Arc<super::super::native_sdk_execution::NativeSdkExecution>,
    },
    Legacy(Option<RetainedExecution>),
}

pub(crate) struct ActiveGatedCustody {
    pub(in crate::engine::container) native_main:
        Option<Arc<super::super::native_sdk_execution::NativeSdkExecution>>,
    pub(in crate::engine::container) native_auth:
        Option<Arc<crate::engine::agent::native_codex_preparation::AuthCustody>>,
    pub(in crate::engine::container) plan: DurableLaunchPlan,
    pub(in crate::engine::container) lifecycle: ChildLifecycleAuthority,
    pub(in crate::engine::container) resource_loan: Option<ExecutionResourceLoan>,
    pub(in crate::engine::container) adapter: Arc<dyn GatedProviderAdapter>,
    pub(in crate::engine::container) last_inspection: Option<ProviderLaunchInspection>,
    pub(in crate::engine::container) reason: LaunchRetentionReason,
    pub(in crate::engine::container) gate_monitor: Option<ActiveGateMonitor>,
    pub(in crate::engine::container) cleanup_phase:
        crate::engine::container::orchestrated_apple::ExactCleanupPhase,
    cleanup_lifecycle: CleanupLifecycleProvenance,
    never_created_reconciliation: Option<Box<NeverCreatedRetentionFailure>>,
    #[cfg(test)]
    pub(in crate::engine::container) boundary: Option<Arc<dyn OrchestratedAppleTestBoundary>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CleanupLifecycleProvenance {
    Created,
    NeverCreatedUncertain,
    NeverCreated,
}

impl ActiveGatedCustody {
    pub(in crate::engine::container) fn new(
        plan: DurableLaunchPlan,
        lifecycle: ChildLifecycleAuthority,
        resource_loan: ExecutionResourceLoan,
        adapter: Arc<dyn GatedProviderAdapter>,
        gate_monitor: Option<ActiveGateMonitor>,
        #[cfg(test)] boundary: Option<Arc<dyn OrchestratedAppleTestBoundary>>,
    ) -> Self {
        Self {
            plan,
            lifecycle,
            resource_loan: Some(resource_loan),
            adapter,
            last_inspection: None,
            reason: LaunchRetentionReason::ChildStateUnknown,
            gate_monitor,
            cleanup_phase:
                crate::engine::container::orchestrated_apple::ExactCleanupPhase::NeedsCleanup,
            native_auth: None,
            native_main: None,
            cleanup_lifecycle: CleanupLifecycleProvenance::Created,
            never_created_reconciliation: None,
            #[cfg(test)]
            boundary,
        }
    }

    pub(in crate::engine::container) fn new_never_created(
        plan: DurableLaunchPlan,
        lifecycle: NeverCreatedChildLifecycle,
        adapter: Arc<dyn GatedProviderAdapter>,
        #[cfg(test)] boundary: Option<Arc<dyn OrchestratedAppleTestBoundary>>,
    ) -> Self {
        let (lifecycle, resource_loan, proof) = lifecycle.into_parts();
        Self {
            plan,
            lifecycle,
            resource_loan: Some(resource_loan),
            adapter,
            last_inspection: None,
            reason: LaunchRetentionReason::SpawnResultUnknown,
            gate_monitor: None,
            cleanup_phase:
                crate::engine::container::orchestrated_apple::ExactCleanupPhase::NeverCreatedNeedsAbsence {
                    proof,
                },
            native_auth: None,
            native_main: None,
            cleanup_lifecycle: CleanupLifecycleProvenance::NeverCreated,
            never_created_reconciliation: None,
            #[cfg(test)]
            boundary,
        }
    }

    pub(in crate::engine::container) fn new_never_created_uncertain(
        plan: DurableLaunchPlan,
        failure: Box<NeverCreatedRetentionFailure>,
        adapter: Arc<dyn GatedProviderAdapter>,
        #[cfg(test)] boundary: Option<Arc<dyn OrchestratedAppleTestBoundary>>,
    ) -> Self {
        let lifecycle = failure.authority();
        Self {
            plan,
            lifecycle,
            resource_loan: None,
            adapter,
            last_inspection: None,
            reason: LaunchRetentionReason::ChildStateUnknown,
            gate_monitor: None,
            cleanup_phase:
                crate::engine::container::orchestrated_apple::ExactCleanupPhase::NeedsCleanup,
            native_auth: None,
            native_main: None,
            cleanup_lifecycle: CleanupLifecycleProvenance::NeverCreatedUncertain,
            never_created_reconciliation: Some(failure),
            #[cfg(test)]
            boundary,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub(crate) struct LaunchRetentionTicket(u64);

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum LaunchRetentionInitError {
    #[error("launch retention supervisor thread unavailable")]
    SupervisorThreadUnavailable,
}

struct Shared {
    entries: Mutex<BTreeMap<LaunchRetentionTicket, RetainedEntry>>,
    tickets: Mutex<BTreeSet<LaunchRetentionTicket>>,
    observation: Mutex<RetentionObservation>,
    changed: Condvar,
    accepting: AtomicBool,
    supervisor_failed: AtomicBool,
    shutdown_requested: AtomicBool,
    worker_detached: AtomicBool,
    worker_finished: AtomicBool,
    next_ticket: AtomicU64,
    active_ticket_cursor: AtomicU64,
    #[cfg(test)]
    resource_loan_releases: Mutex<BTreeMap<LaunchRetentionTicket, usize>>,
    #[cfg(test)]
    supervisor_active_claims_blocked: AtomicBool,
    #[cfg(test)]
    drop_on_worker: Mutex<Option<Arc<LaunchRetentionRegistry>>>,
}

struct RetainedEntry {
    launch: RetainedRegistryEnvelope,
    // Local CLI retirement cannot release unresolved main provider custody.
    _native_main: Option<Arc<NativeSdkExecution>>,
    raw_custody: Option<RawCustodyCapability>,
    #[cfg_attr(not(test), allow(dead_code))]
    raw_counters: (usize, usize, usize, usize),
    signal_authorized: bool,
    owns_unreaped_child: bool,
}

#[derive(Default)]
struct RetentionObservation {
    retained: usize,
    unreaped: usize,
    #[cfg(test)]
    unbound_attempts: usize,
    #[cfg(test)]
    unbound_pending: usize,
    #[cfg(test)]
    unbound_errors: usize,
    #[cfg(test)]
    unbound_reaped: usize,
    #[cfg(test)]
    unbound_kill_calls: usize,
    #[cfg(test)]
    managed_prepared: usize,
    #[cfg(test)]
    managed_running: usize,
    #[cfg(test)]
    managed_failed: usize,
    #[cfg(test)]
    managed_exited: usize,
    #[cfg(test)]
    last_pid: Option<u32>,
    #[cfg(test)]
    last_errno: Option<i32>,
    #[cfg(test)]
    last_resources_present: bool,
}

pub(crate) struct LaunchRetentionRegistry {
    shared: Arc<Shared>,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl fmt::Debug for LaunchRetentionRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LaunchRetentionRegistry([redacted])")
    }
}

impl LaunchRetentionRegistry {
    pub(crate) fn try_new() -> Result<Arc<Self>, LaunchRetentionInitError> {
        Self::new_inner(false)
    }

    fn new_inner(fail_thread: bool) -> Result<Arc<Self>, LaunchRetentionInitError> {
        if fail_thread {
            return Err(LaunchRetentionInitError::SupervisorThreadUnavailable);
        }
        let shared = Arc::new(Shared {
            entries: Mutex::new(BTreeMap::new()),
            tickets: Mutex::new(BTreeSet::new()),
            observation: Mutex::new(RetentionObservation::default()),
            changed: Condvar::new(),
            accepting: AtomicBool::new(true),
            supervisor_failed: AtomicBool::new(false),
            shutdown_requested: AtomicBool::new(false),
            worker_detached: AtomicBool::new(false),
            worker_finished: AtomicBool::new(false),
            next_ticket: AtomicU64::new(1),
            active_ticket_cursor: AtomicU64::new(0),
            #[cfg(test)]
            resource_loan_releases: Mutex::new(BTreeMap::new()),
            #[cfg(test)]
            supervisor_active_claims_blocked: AtomicBool::new(false),
            #[cfg(test)]
            drop_on_worker: Mutex::new(None),
        });
        let worker_shared = Arc::clone(&shared);
        let worker = std::thread::Builder::new()
            .name("awman-launch-retention".into())
            .spawn(move || supervisor_loop(worker_shared))
            .map_err(|_| LaunchRetentionInitError::SupervisorThreadUnavailable)?;
        Ok(Arc::new(Self {
            shared,
            worker: Mutex::new(Some(worker)),
        }))
    }

    pub(crate) fn retain(&self, launch: RetainedAgentLaunch) -> LaunchRetentionTicket {
        self.retain_envelope(RetainedRegistryEnvelope::Gated(Box::new(launch)), None)
    }

    pub(in crate::engine::container) fn retain_native_main_after_start(
        &self,
        launch: RetainedAgentLaunch,
        main: Arc<NativeSdkExecution>,
    ) -> LaunchRetentionTicket {
        self.retain_envelope(
            RetainedRegistryEnvelope::Gated(Box::new(launch)),
            Some(main),
        )
    }

    pub(in crate::engine::container) fn retain_legacy(
        &self,
        execution: RetainedExecution,
    ) -> LaunchRetentionTicket {
        self.retain_envelope(RetainedRegistryEnvelope::Legacy(Some(execution)), None)
    }

    pub(in crate::engine::container) fn retain_active_gated(
        &self,
        custody: ActiveGatedCustody,
    ) -> LaunchRetentionTicket {
        let ticket = self.retain_envelope(
            RetainedRegistryEnvelope::ActiveGated(Box::new(custody)),
            None,
        );
        #[cfg(test)]
        self.shared
            .resource_loan_releases
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(ticket)
            .or_insert(0);
        ticket
    }

    pub(in crate::engine::container) fn retain_unresolved_auth_preparation(
        &self,
        prepared: PreparedChildLifecycle,
        resources: Box<dyn Any + Send>,
        auth: Arc<crate::engine::agent::native_codex_preparation::AuthCustody>,
    ) -> LaunchRetentionTicket {
        self.retain_envelope(
            RetainedRegistryEnvelope::UnresolvedAuthPreparation {
                _prepared: prepared,
                _resources: resources,
                _auth: auth,
            },
            None,
        )
    }

    pub(in crate::engine) fn retain_sdk_completion(
        &self,
        owner: crate::engine::agent::native_codex_preparation::SdkCompletionCustody,
    ) -> LaunchRetentionTicket {
        self.retain_envelope(
            RetainedRegistryEnvelope::SdkCompletionUncertain(owner),
            None,
        )
    }
    pub(in crate::engine::container) fn retain_unresolved_main_preparation(
        &self,
        prepared: PreparedChildLifecycle,
        resources: Box<dyn Any + Send>,
        main: Arc<super::super::native_sdk_execution::NativeSdkExecution>,
    ) -> LaunchRetentionTicket {
        self.retain_envelope(
            RetainedRegistryEnvelope::UnresolvedMainPreparation {
                _prepared: prepared,
                _resources: resources,
                _main: main,
            },
            None,
        )
    }
    #[allow(dead_code)]
    pub(crate) fn reconcile_sdk_completion(
        &self,
        ticket: LaunchRetentionTicket,
        deadline: Instant,
    ) -> Result<SdkCompletionReconcile, EngineError> {
        let mut entry = match claim_outstanding_entry(&self.shared, ticket, deadline)? {
            OutstandingEntryClaim::Claimed(entry) => *entry,
            OutstandingEntryClaim::Resolved => {
                return Err(EngineError::Container(
                    "retained SDK completion ticket unavailable".into(),
                ))
            }
            OutstandingEntryClaim::RetainedInFlight => {
                return Ok(SdkCompletionReconcile::Retained {
                    ticket,
                    error: EngineError::Container("retained SDK completion busy".into()),
                })
            }
        };
        let RetainedRegistryEnvelope::SdkCompletionUncertain(owner) = &mut entry.launch else {
            publish_retained_entry(&self.shared, ticket, entry);
            return Err(EngineError::Container(
                "ticket does not retain SDK completion".into(),
            ));
        };
        match owner.reconcile() {
            Ok(()) => {
                record_poll_disposition(
                    &self.shared,
                    &mut entry,
                    RetainedEntryPoll {
                        child_reaped: false,
                        resolved: true,
                    },
                );
                publish_resolved_entry(&self.shared, ticket);
                Ok(SdkCompletionReconcile::Completed)
            }
            Err(error) => {
                publish_retained_entry(&self.shared, ticket, entry);
                Ok(SdkCompletionReconcile::Retained { ticket, error })
            }
        }
    }

    pub(in crate::engine::container) fn retain_pending_publication(
        &self,
        publication: Box<PendingPublication>,
        prepared: PreparedChildLifecycle,
        resources: Box<dyn Any + Send>,
        adapter: Arc<dyn GatedProviderAdapter>,
        #[cfg(test)] boundary: Option<Arc<dyn OrchestratedAppleTestBoundary>>,
    ) -> LaunchRetentionTicket {
        let ticket = self.retain_envelope(
            RetainedRegistryEnvelope::PendingPublication {
                publication: Some(publication),
                prepared: Some(prepared),
                resources: Some(resources),
                adapter,
                #[cfg(test)]
                boundary,
            },
            None,
        );
        #[cfg(test)]
        self.shared
            .resource_loan_releases
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(ticket)
            .or_insert(0);
        ticket
    }

    // Exact-ticket callers share the same operation now used by the production supervisor.
    #[allow(dead_code)]
    pub(crate) fn retry_active_gated(
        &self,
        ticket: LaunchRetentionTicket,
        deadline: Instant,
    ) -> Result<ActiveGatedRetryOutcome, EngineError> {
        let mut entry = match claim_outstanding_entry(&self.shared, ticket, deadline)? {
            OutstandingEntryClaim::Claimed(entry) => *entry,
            OutstandingEntryClaim::Resolved => return Ok(ActiveGatedRetryOutcome::Resolved),
            OutstandingEntryClaim::RetainedInFlight => {
                return Ok(ActiveGatedRetryOutcome::Retained);
            }
        };
        match retry_claimed_active_gated(&self.shared, ticket, &mut entry, deadline) {
            Ok(ActiveGatedRetryOutcome::Resolved) => {
                publish_resolved_entry(&self.shared, ticket);
                Ok(ActiveGatedRetryOutcome::Resolved)
            }
            Ok(ActiveGatedRetryOutcome::Retained) => {
                publish_retained_entry(&self.shared, ticket, entry);
                Ok(ActiveGatedRetryOutcome::Retained)
            }
            Err(error) => {
                publish_retained_entry(&self.shared, ticket, entry);
                Err(error)
            }
        }
    }

    fn retain_envelope(
        &self,
        launch: RetainedRegistryEnvelope,
        native_main: Option<Arc<NativeSdkExecution>>,
    ) -> LaunchRetentionTicket {
        // Classify custody before taking registry locks. A successful bind may
        // still be queued while the actor reports Prepared, so only an actual
        // published Exited state proves that managed custody is reaped.
        let owns_unreaped_child = initially_owns_unreaped_child(&launch);
        let ticket = LaunchRetentionTicket(self.shared.next_ticket.fetch_add(1, Ordering::Relaxed));
        let mut entries = self.shared.entries.lock().unwrap_or_else(|poisoned| {
            self.shared.supervisor_failed.store(true, Ordering::Release);
            poisoned.into_inner()
        });
        entries.insert(
            ticket,
            RetainedEntry {
                launch,
                _native_main: native_main,
                raw_custody: None,
                raw_counters: (0, 0, 0, 0),
                signal_authorized: true,
                owns_unreaped_child,
            },
        );
        self.shared
            .tickets
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(ticket);
        {
            // entries is the publication exclusion: the supervisor cannot
            // poll and reap this entry before both counters are installed.
            let mut observation = self.shared.observation.lock().unwrap_or_else(|poisoned| {
                self.shared.supervisor_failed.store(true, Ordering::Release);
                poisoned.into_inner()
            });
            observation.retained = observation.retained.saturating_add(1);
            if owns_unreaped_child {
                observation.unreaped = observation.unreaped.saturating_add(1);
            }
        }
        drop(entries);
        self.shared.changed.notify_all();
        ticket
    }
}

enum OutstandingEntryClaim {
    Claimed(Box<RetainedEntry>),
    Resolved,
    RetainedInFlight,
}

fn claim_outstanding_entry(
    shared: &Shared,
    ticket: LaunchRetentionTicket,
    deadline: Instant,
) -> Result<OutstandingEntryClaim, EngineError> {
    let mut entries = shared.entries.lock().unwrap_or_else(|poisoned| {
        shared.supervisor_failed.store(true, Ordering::Release);
        poisoned.into_inner()
    });
    let mut observed_outstanding = false;
    loop {
        if let Some(entry) = entries.remove(&ticket) {
            return Ok(OutstandingEntryClaim::Claimed(Box::new(entry)));
        }
        let ticket_present = shared
            .tickets
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains(&ticket);
        if !ticket_present {
            return if observed_outstanding {
                Ok(OutstandingEntryClaim::Resolved)
            } else {
                Err(EngineError::Container(
                    "retained launch ticket unavailable".into(),
                ))
            };
        }
        observed_outstanding = true;
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Ok(OutstandingEntryClaim::RetainedInFlight);
        };
        let (next, _) = shared
            .changed
            .wait_timeout(entries, remaining)
            .unwrap_or_else(|p| p.into_inner());
        entries = next;
    }
}

fn retry_claimed_active_gated(
    shared: &Shared,
    _ticket: LaunchRetentionTicket,
    entry: &mut RetainedEntry,
    deadline: Instant,
) -> Result<ActiveGatedRetryOutcome, EngineError> {
    if matches!(
        entry.launch,
        RetainedRegistryEnvelope::UnresolvedAuthPreparation { .. }
            | RetainedRegistryEnvelope::UnresolvedMainPreparation { .. }
            | RetainedRegistryEnvelope::SdkCompletionUncertain(_)
    ) {
        return Ok(ActiveGatedRetryOutcome::Retained);
    }
    if let RetainedRegistryEnvelope::PendingPublication {
        publication,
        prepared,
        resources,
        adapter,
        #[cfg(test)]
        boundary,
    } = &mut entry.launch
    {
        let Some(pending) = publication.take() else {
            return Err(EngineError::Container(
                "retained pending publication custody unavailable".into(),
            ));
        };
        let Some(prepared_owner) = prepared.take() else {
            *publication = Some(pending);
            return Err(EngineError::Container(
                "retained pending publication custody unavailable".into(),
            ));
        };
        let Some(resource_owner) = resources.take() else {
            *publication = Some(pending);
            *prepared = Some(prepared_owner);
            return Err(EngineError::Container(
                "retained pending publication custody unavailable".into(),
            ));
        };
        let plan = match (*pending).try_promote() {
            Ok(plan) => plan,
            Err(pending) => {
                *publication = Some(pending);
                *prepared = Some(prepared_owner);
                *resources = Some(resource_owner);
                return Ok(ActiveGatedRetryOutcome::Retained);
            }
        };
        let adapter = Arc::clone(adapter);
        #[cfg(test)]
        let boundary = boundary.clone();
        let custody = match prepared_owner.retain_never_created_resources(resource_owner) {
            Ok(lifecycle) => {
                #[cfg(test)]
                if let Some(boundary) = boundary.as_deref() {
                    boundary.observe_never_created_proven();
                }
                ActiveGatedCustody::new_never_created(
                    plan,
                    lifecycle,
                    adapter,
                    #[cfg(test)]
                    boundary,
                )
            }
            Err(failure) => ActiveGatedCustody::new_never_created_uncertain(
                plan,
                failure,
                adapter,
                #[cfg(test)]
                boundary,
            ),
        };
        entry.launch = RetainedRegistryEnvelope::ActiveGated(Box::new(custody));
        return Ok(ActiveGatedRetryOutcome::Retained);
    }

    let RetainedRegistryEnvelope::ActiveGated(custody) = &mut entry.launch else {
        return Err(EngineError::Container(
            "retained launch ticket is not an active gated launch".into(),
        ));
    };

    if custody.cleanup_lifecycle == CleanupLifecycleProvenance::NeverCreatedUncertain {
        let Some(failure) = custody.never_created_reconciliation.take() else {
            custody.reason = LaunchRetentionReason::ChildStateUnknown;
            return Err(EngineError::Container(
                "retained never-created reconciliation unavailable".into(),
            ));
        };
        let lifecycle = match (*failure).retry() {
            Ok(lifecycle) => lifecycle,
            Err(failure) => {
                custody.never_created_reconciliation = Some(failure);
                custody.reason = LaunchRetentionReason::ChildStateUnknown;
                return Ok(ActiveGatedRetryOutcome::Retained);
            }
        };
        let (lifecycle, resource_loan, proof) = lifecycle.into_parts();
        custody.lifecycle = lifecycle;
        custody.resource_loan = Some(resource_loan);
        custody.cleanup_phase =
            crate::engine::container::orchestrated_apple::ExactCleanupPhase::NeverCreatedNeedsAbsence {
                proof,
            };
        custody.cleanup_lifecycle = CleanupLifecycleProvenance::NeverCreated;
        custody.reason = LaunchRetentionReason::SpawnResultUnknown;
        #[cfg(test)]
        if let Some(boundary) = custody.boundary.as_deref() {
            boundary.observe_never_created_proven();
        }
        return Ok(ActiveGatedRetryOutcome::Retained);
    }

    if matches!(custody.lifecycle.state(), Ok(ChildLifecycleState::Prepared)) {
        let positively_never_created = custody.cleanup_lifecycle
            == CleanupLifecycleProvenance::NeverCreated
            && matches!(
                custody.cleanup_phase,
                crate::engine::container::orchestrated_apple::ExactCleanupPhase::NeverCreatedNeedsAbsence { .. }
                    | crate::engine::container::orchestrated_apple::ExactCleanupPhase::PublicationPending {
                        last_inspection: None,
                        ..
                    }
                    | crate::engine::container::orchestrated_apple::ExactCleanupPhase::Complete
            );
        if !positively_never_created {
            return Ok(ActiveGatedRetryOutcome::Retained);
        }
    }

    if let Some(monitor) = custody.gate_monitor.as_mut() {
        if !monitor.cancel_and_wait_until(deadline) {
            return Ok(ActiveGatedRetryOutcome::Retained);
        }
        custody.gate_monitor = None;
    }

    if custody.cleanup_phase
        != crate::engine::container::orchestrated_apple::ExactCleanupPhase::Complete
    {
        let cleanup = crate::engine::container::orchestrated_apple::cleanup_exact_launch(
            &custody.plan,
            custody.adapter.as_ref(),
            &custody.lifecycle,
            &mut custody.cleanup_phase,
            deadline,
            #[cfg(test)]
            custody.boundary.as_deref(),
        );
        if let Err(error) = cleanup {
            let (reason, last_inspection) = *error;
            custody.reason = reason;
            if last_inspection.is_some() {
                custody.last_inspection = last_inspection;
            }
            let child_reaped = custody.lifecycle.retained_child_is_actually_reaped();
            record_poll_disposition(
                shared,
                entry,
                RetainedEntryPoll {
                    child_reaped,
                    resolved: false,
                },
            );
            return Ok(ActiveGatedRetryOutcome::Retained);
        }
    }

    let already_released_auth = (custody.native_auth.is_some() || custody.native_main.is_some())
        && custody.resource_loan.is_none()
        && custody.lifecycle.resources_are_released();
    let loan_released = already_released_auth
        || custody
            .resource_loan
            .as_mut()
            .is_some_and(|resource_loan| resource_loan.release_and_wait_until(deadline));
    if !loan_released {
        return Ok(ActiveGatedRetryOutcome::Retained);
    }

    let _released_resource_loan = custody.resource_loan.take();
    if _released_resource_loan.is_none() && !already_released_auth {
        return Err(EngineError::Container(
            "retained execution resource loan unavailable".into(),
        ));
    }
    #[cfg(test)]
    {
        let mut releases = shared
            .resource_loan_releases
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let count = releases.entry(_ticket).or_default();
        *count = count.saturating_add(1);
    }
    #[cfg(test)]
    if let Some(boundary) = custody.boundary.as_deref() {
        boundary.observe_resource_loan_released();
    }

    record_poll_disposition(
        shared,
        entry,
        RetainedEntryPoll {
            child_reaped: true,
            resolved: true,
        },
    );
    Ok(ActiveGatedRetryOutcome::Resolved)
}

fn publish_retained_entry(shared: &Shared, ticket: LaunchRetentionTicket, entry: RetainedEntry) {
    shared
        .entries
        .lock()
        .unwrap_or_else(|poisoned| {
            shared.supervisor_failed.store(true, Ordering::Release);
            poisoned.into_inner()
        })
        .insert(ticket, entry);
    shared.changed.notify_all();
}

fn publish_resolved_entry(shared: &Shared, ticket: LaunchRetentionTicket) {
    let entries = shared.entries.lock().unwrap_or_else(|poisoned| {
        shared.supervisor_failed.store(true, Ordering::Release);
        poisoned.into_inner()
    });
    shared
        .tickets
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(&ticket);
    drop(entries);
    shared.changed.notify_all();
}

impl Drop for LaunchRetentionRegistry {
    fn drop(&mut self) {
        self.shared.accepting.store(false, Ordering::Release);
        self.shared
            .shutdown_requested
            .store(true, Ordering::Release);
        self.shared.changed.notify_all();
        let deadline = Instant::now() + REGISTRY_SHUTDOWN_GRACE;
        let worker = self.worker.lock().unwrap_or_else(|p| p.into_inner()).take();
        let Some(worker) = worker else { return };
        if worker.thread().id() == std::thread::current().id() {
            self.shared.worker_detached.store(true, Ordering::Release);
            drop(worker);
            return;
        }
        while !worker.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        if worker.is_finished() {
            let _ = worker.join();
        } else {
            self.shared.worker_detached.store(true, Ordering::Release);
            drop(worker);
        }
    }
}

fn supervisor_loop(shared: Arc<Shared>) {
    loop {
        #[cfg(test)]
        {
            let registry = shared
                .drop_on_worker
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .take();
            drop(registry);
        }
        let shutdown = shared.shutdown_requested.load(Ordering::Acquire);
        let _ = supervisor_iteration(&shared, shutdown, true);
        let empty = shared
            .entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_empty();
        if shutdown && empty {
            break;
        }
        let guard = shared.entries.lock().unwrap_or_else(|p| p.into_inner());
        let _ = shared.changed.wait_timeout(guard, SUPERVISOR_POLL_INTERVAL);
    }
    shared.worker_finished.store(true, Ordering::Release);
}

fn next_active_ticket(
    entries: &BTreeMap<LaunchRetentionTicket, RetainedEntry>,
    after: u64,
) -> Option<LaunchRetentionTicket> {
    entries
        .iter()
        .find_map(|(ticket, entry)| {
            (ticket.0 > after && is_supervisor_active(&entry.launch)).then_some(*ticket)
        })
        .or_else(|| {
            entries
                .iter()
                .find_map(|(ticket, entry)| is_supervisor_active(&entry.launch).then_some(*ticket))
        })
}

fn is_supervisor_active(launch: &RetainedRegistryEnvelope) -> bool {
    matches!(
        launch,
        RetainedRegistryEnvelope::PendingPublication { .. }
            | RetainedRegistryEnvelope::ActiveGated(_)
    )
}

fn supervisor_iteration(
    shared: &Shared,
    shutdown: bool,
    #[cfg_attr(not(test), allow(unused_variables))] honor_active_claim_barrier: bool,
) -> Result<Option<LaunchRetentionTicket>, EngineError> {
    let mut local = Vec::new();
    let active = {
        let mut entries = shared.entries.lock().unwrap_or_else(|poisoned| {
            shared.supervisor_failed.store(true, Ordering::Release);
            poisoned.into_inner()
        });
        #[cfg(test)]
        let active_claims_blocked = honor_active_claim_barrier
            && shared
                .supervisor_active_claims_blocked
                .load(Ordering::Acquire);
        #[cfg(not(test))]
        let active_claims_blocked = false;
        let active_ticket = if active_claims_blocked {
            None
        } else {
            next_active_ticket(
                &entries,
                shared.active_ticket_cursor.load(Ordering::Acquire),
            )
        };
        if let Some(ticket) = active_ticket {
            shared
                .active_ticket_cursor
                .store(ticket.0, Ordering::Release);
        }
        let active =
            active_ticket.and_then(|ticket| entries.remove(&ticket).map(|entry| (ticket, entry)));
        let tickets: Vec<_> = entries
            .iter()
            .filter_map(|(ticket, entry)| (!is_supervisor_active(&entry.launch)).then_some(*ticket))
            .collect();
        for ticket in tickets {
            if let Some(entry) = entries.remove(&ticket) {
                local.push((ticket, entry));
            }
        }
        active
    };
    let mut keep = Vec::new();
    for (ticket, mut entry) in local {
        let disposition = poll_retained_entry(shared, &mut entry, shutdown);
        record_poll_disposition(shared, &mut entry, disposition);
        if !disposition.resolved {
            keep.push((ticket, entry));
        } else {
            publish_resolved_entry(shared, ticket);
        }
    }
    if !keep.is_empty() {
        let mut entries = shared.entries.lock().unwrap_or_else(|poisoned| {
            shared.supervisor_failed.store(true, Ordering::Release);
            poisoned.into_inner()
        });
        entries.extend(keep);
    }
    let Some((ticket, mut entry)) = active else {
        return Ok(None);
    };
    let deadline = Instant::now() + SUPERVISOR_ACTIVE_RETRY_BOUND;
    match retry_claimed_active_gated(shared, ticket, &mut entry, deadline) {
        Ok(ActiveGatedRetryOutcome::Resolved) => {
            publish_resolved_entry(shared, ticket);
            Ok(Some(ticket))
        }
        Ok(ActiveGatedRetryOutcome::Retained) => {
            publish_retained_entry(shared, ticket, entry);
            Ok(Some(ticket))
        }
        Err(error) => {
            shared.supervisor_failed.store(true, Ordering::Release);
            publish_retained_entry(shared, ticket, entry);
            Err(error)
        }
    }
}

#[derive(Clone, Copy)]
struct RetainedEntryPoll {
    child_reaped: bool,
    resolved: bool,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum RetainedChildPoll {
    Pending,
    UnknownError,
    Reaped,
}

fn initially_owns_unreaped_child(launch: &RetainedRegistryEnvelope) -> bool {
    let execution = match launch {
        RetainedRegistryEnvelope::Gated(launch) => launch.execution.as_ref(),
        RetainedRegistryEnvelope::PendingPublication { .. } => return false,
        // Only local non-start is known; this says nothing about the provider.
        RetainedRegistryEnvelope::UnresolvedAuthPreparation { .. }
        | RetainedRegistryEnvelope::UnresolvedMainPreparation { .. }
        | RetainedRegistryEnvelope::SdkCompletionUncertain(_) => return false,
        RetainedRegistryEnvelope::ActiveGated(launch) => {
            return match launch.cleanup_lifecycle {
                CleanupLifecycleProvenance::Created => {
                    !launch.lifecycle.retained_child_is_actually_reaped()
                }
                CleanupLifecycleProvenance::NeverCreatedUncertain
                | CleanupLifecycleProvenance::NeverCreated => false,
            };
        }
        RetainedRegistryEnvelope::Legacy(execution) => execution.as_ref(),
    };
    execution.is_some_and(|execution| match execution {
        RetainedExecution::Managed { lifecycle, .. } => {
            !lifecycle.retained_child_is_actually_reaped()
        }
        RetainedExecution::Unbound(_) | RetainedExecution::OwnedUnbound { .. } => true,
    })
}

fn record_poll_disposition(
    shared: &Shared,
    entry: &mut RetainedEntry,
    disposition: RetainedEntryPoll,
) {
    let child_reaped = disposition.child_reaped && entry.owns_unreaped_child;
    if !child_reaped && !disposition.resolved {
        return;
    }
    let mut observation = shared.observation.lock().unwrap_or_else(|poisoned| {
        shared.supervisor_failed.store(true, Ordering::Release);
        poisoned.into_inner()
    });
    if child_reaped {
        if let Some(unreaped) = observation.unreaped.checked_sub(1) {
            observation.unreaped = unreaped;
            entry.owns_unreaped_child = false;
        } else {
            shared.supervisor_failed.store(true, Ordering::Release);
        }
    }
    if disposition.resolved {
        if let Some(retained) = observation.retained.checked_sub(1) {
            observation.retained = retained;
        } else {
            shared.supervisor_failed.store(true, Ordering::Release);
        }
    }
}

fn poll_retained_entry(
    shared: &Shared,
    entry: &mut RetainedEntry,
    shutdown: bool,
) -> RetainedEntryPoll {
    match &mut entry.launch {
        RetainedRegistryEnvelope::Gated(launch) => {
            let child_reaped = poll_execution_slot(
                shared,
                &mut launch.execution,
                &mut entry.raw_custody,
                &mut entry.raw_counters,
                shutdown,
                &mut entry.signal_authorized,
            ) == RetainedChildPoll::Reaped;
            // Reaping the local CLI does not discharge provider, control,
            // journal, plan, pin, inspection, or reason custody.
            RetainedEntryPoll {
                child_reaped,
                resolved: false,
            }
        }
        RetainedRegistryEnvelope::PendingPublication { .. } => RetainedEntryPoll {
            child_reaped: true,
            resolved: false,
        },
        RetainedRegistryEnvelope::UnresolvedAuthPreparation { .. }
        | RetainedRegistryEnvelope::UnresolvedMainPreparation { .. }
        | RetainedRegistryEnvelope::SdkCompletionUncertain(_) => RetainedEntryPoll {
            child_reaped: false,
            resolved: false,
        },
        RetainedRegistryEnvelope::ActiveGated(launch) => {
            let child_reaped = launch.lifecycle.retained_child_is_actually_reaped();
            RetainedEntryPoll {
                child_reaped,
                resolved: false,
            }
        }
        RetainedRegistryEnvelope::Legacy(execution) => {
            match poll_execution_slot(
                shared,
                execution,
                &mut entry.raw_custody,
                &mut entry.raw_counters,
                shutdown,
                &mut entry.signal_authorized,
            ) {
                RetainedChildPoll::Pending | RetainedChildPoll::UnknownError => RetainedEntryPoll {
                    child_reaped: false,
                    resolved: false,
                },
                RetainedChildPoll::Reaped => RetainedEntryPoll {
                    child_reaped: true,
                    resolved: true,
                },
            }
        }
    }
}

fn poll_execution_slot(
    shared: &Shared,
    execution: &mut Option<RetainedExecution>,
    raw_custody: &mut Option<RawCustodyCapability>,
    raw_counters: &mut (usize, usize, usize, usize),
    shutdown: bool,
    signal_authorized: &mut bool,
) -> RetainedChildPoll {
    if let Some(raw) = raw_custody.as_ref() {
        return poll_raw_custody(shared, raw, raw_counters, signal_authorized);
    }
    let is_raw = matches!(
        execution,
        Some(RetainedExecution::Unbound(_) | RetainedExecution::OwnedUnbound { .. })
    );
    if is_raw {
        let Some(owned) = execution.take() else {
            return RetainedChildPoll::Pending;
        };
        let handed_off = match owned {
            RetainedExecution::Unbound(child) => child.handoff_raw(None),
            RetainedExecution::OwnedUnbound { child, resources } => {
                child.handoff_raw(Some(resources))
            }
            managed @ RetainedExecution::Managed { .. } => {
                *execution = Some(managed);
                return RetainedChildPoll::Pending;
            }
        };
        *raw_custody = Some(handed_off);
        let Some(raw) = raw_custody.as_ref() else {
            return RetainedChildPoll::Pending;
        };
        return poll_raw_custody(shared, raw, raw_counters, signal_authorized);
    }
    let Some(managed) = execution.as_mut() else {
        return RetainedChildPoll::Reaped;
    };
    let outcome = poll_managed_execution(shared, managed, shutdown, signal_authorized);
    if outcome == RetainedChildPoll::Reaped {
        *execution = None;
    }
    outcome
}

fn poll_managed_execution(
    _shared: &Shared,
    execution: &mut RetainedExecution,
    _shutdown: bool,
    signal_authorized: &mut bool,
) -> RetainedChildPoll {
    match execution {
        RetainedExecution::Managed {
            execution: managed_execution,
            lifecycle,
        } => {
            let state = lifecycle.state();
            #[cfg(test)]
            record_managed_observation(_shared, state.as_ref().ok());
            let termination_ready = matches!(state.as_ref(), Ok(ChildLifecycleState::Running));
            if *signal_authorized && termination_ready {
                match lifecycle.request_terminate_local_cli(
                    Instant::now() + RETAINED_LOCAL_CLI_TERMINATE_BOUND,
                ) {
                    TerminateRequestOutcome::Exited(_)
                    | TerminateRequestOutcome::AcceptedPending(_) => {
                        *signal_authorized = false;
                    }
                    TerminateRequestOutcome::NotAccepted(_) => {}
                }
            }
            match state {
                Ok(ChildLifecycleState::Exited(_)) => {
                    // Dropping a successful execution returns its unique loan.
                    // Keep the lifecycle entry until the custody worker has
                    // synchronously completed the owned resource teardown.
                    // A pre-execution retained value has no loan; resolving it
                    // drops this registry's command authority so the actor can
                    // finish, after which the no-loan custody path may clean up.
                    *managed_execution = None;
                    if lifecycle.execution_loan_was_issued() && !lifecycle.resources_are_released()
                    {
                        RetainedChildPoll::Pending
                    } else {
                        RetainedChildPoll::Reaped
                    }
                }
                Ok(
                    ChildLifecycleState::Prepared
                    | ChildLifecycleState::Running
                    | ChildLifecycleState::Failed,
                )
                | Err(_) => RetainedChildPoll::Pending,
            }
        }
        RetainedExecution::Unbound(_) | RetainedExecution::OwnedUnbound { .. } => {
            RetainedChildPoll::Pending
        }
    }
}

fn poll_raw_custody(
    _shared: &Shared,
    raw: &RawCustodyCapability,
    _raw_counters: &mut (usize, usize, usize, usize),
    signal_authorized: &mut bool,
) -> RetainedChildPoll {
    if *signal_authorized {
        let first_request = raw.request_kill();
        #[cfg(test)]
        if first_request {
            let mut observation = _shared.observation.lock().unwrap_or_else(|poisoned| {
                _shared.supervisor_failed.store(true, Ordering::Release);
                poisoned.into_inner()
            });
            observation.unbound_kill_calls = observation.unbound_kill_calls.saturating_add(1);
        }
        #[cfg(not(test))]
        let _ = first_request;
        *signal_authorized = false;
    }
    let snapshot = raw.snapshot();
    #[cfg(test)]
    record_unbound_observation(_shared, raw, _raw_counters, snapshot);
    match snapshot.outcome {
        RawCustodyOutcome::Pending => RetainedChildPoll::Pending,
        RawCustodyOutcome::Reaped => RetainedChildPoll::Reaped,
        RawCustodyOutcome::UnknownError { .. } => RetainedChildPoll::UnknownError,
    }
}

#[cfg(test)]
fn record_unbound_observation(
    shared: &Shared,
    raw: &RawCustodyCapability,
    raw_counters: &mut (usize, usize, usize, usize),
    snapshot: super::child_lifecycle::RawCustodySnapshot,
) {
    let current = raw.counters();
    let delta = (
        current.0.saturating_sub(raw_counters.0),
        current.1.saturating_sub(raw_counters.1),
        current.2.saturating_sub(raw_counters.2),
        current.3.saturating_sub(raw_counters.3),
    );
    *raw_counters = current;
    let mut observation = shared.observation.lock().unwrap_or_else(|poisoned| {
        shared.supervisor_failed.store(true, Ordering::Release);
        poisoned.into_inner()
    });
    observation.unbound_attempts = observation.unbound_attempts.saturating_add(delta.0);
    observation.unbound_pending = observation.unbound_pending.saturating_add(delta.1);
    observation.unbound_errors = observation.unbound_errors.saturating_add(delta.2);
    observation.unbound_reaped = observation.unbound_reaped.saturating_add(delta.3);
    observation.last_pid = snapshot.pid;
    observation.last_resources_present = snapshot.resources_present;
    observation.last_errno = match snapshot.outcome {
        RawCustodyOutcome::UnknownError { errno } => errno,
        RawCustodyOutcome::Pending | RawCustodyOutcome::Reaped => None,
    };
}

#[cfg(test)]
fn record_managed_observation(shared: &Shared, state: Option<&ChildLifecycleState>) {
    let mut observation = shared.observation.lock().unwrap_or_else(|poisoned| {
        shared.supervisor_failed.store(true, Ordering::Release);
        poisoned.into_inner()
    });
    match state {
        Some(ChildLifecycleState::Prepared) => {
            observation.managed_prepared = observation.managed_prepared.saturating_add(1)
        }
        Some(ChildLifecycleState::Running) => {
            observation.managed_running = observation.managed_running.saturating_add(1)
        }
        Some(ChildLifecycleState::Failed) => {
            observation.managed_failed = observation.managed_failed.saturating_add(1)
        }
        Some(ChildLifecycleState::Exited(_)) => {
            observation.managed_exited = observation.managed_exited.saturating_add(1)
        }
        None => {}
    }
}

#[cfg(test)]
#[path = "retention_test_support.rs"]
pub(crate) mod test_support;
