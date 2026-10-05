use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::child_lifecycle::{
    ChildLifecycleState, RetainedExecution, SpawnedCreateCli, UnboundStartedCli,
};
use super::{DurableLaunchPlan, ProviderLaunchInspection};
#[cfg(test)]
use crate::engine::error::EngineError;

pub(crate) const REGISTRY_SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
const SUPERVISOR_POLL_INTERVAL: Duration = Duration::from_millis(20);
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

enum RetainedRegistryEnvelope {
    Gated(Box<RetainedAgentLaunch>),
    Legacy(RetainedExecution),
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
    #[cfg(test)]
    drop_on_worker: Mutex<Option<Arc<LaunchRetentionRegistry>>>,
}

struct RetainedEntry {
    launch: RetainedRegistryEnvelope,
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
        self.retain_envelope(RetainedRegistryEnvelope::Gated(Box::new(launch)))
    }

    pub(in crate::engine::container) fn retain_legacy(
        &self,
        execution: RetainedExecution,
    ) -> LaunchRetentionTicket {
        self.retain_envelope(RetainedRegistryEnvelope::Legacy(execution))
    }

    fn retain_envelope(&self, launch: RetainedRegistryEnvelope) -> LaunchRetentionTicket {
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
        let mut local = Vec::new();
        {
            let mut entries = shared.entries.lock().unwrap_or_else(|poisoned| {
                shared.supervisor_failed.store(true, Ordering::Release);
                poisoned.into_inner()
            });
            let tickets: Vec<_> = entries.keys().copied().collect();
            for ticket in tickets {
                if let Some(entry) = entries.remove(&ticket) {
                    local.push((ticket, entry));
                }
            }
        }
        let mut keep = Vec::new();
        for (ticket, mut entry) in local {
            let disposition = poll_retained_entry(&shared, &mut entry, shutdown);
            record_poll_disposition(&shared, &mut entry, disposition);
            if !disposition.resolved {
                keep.push((ticket, entry));
            } else {
                shared
                    .tickets
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&ticket);
            }
        }
        if !keep.is_empty() {
            let mut entries = shared.entries.lock().unwrap_or_else(|poisoned| {
                shared.supervisor_failed.store(true, Ordering::Release);
                poisoned.into_inner()
            });
            entries.extend(keep);
        }
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
        RetainedRegistryEnvelope::Legacy(execution) => Some(execution),
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
            let mut child_reaped = false;
            if let Some(execution) = launch.execution.as_mut() {
                if poll_retained_execution(
                    shared,
                    execution,
                    shutdown,
                    &mut entry.signal_authorized,
                ) == RetainedChildPoll::Reaped
                {
                    launch.execution = None;
                    child_reaped = true;
                }
            }
            // Reaping the local CLI does not discharge provider, control,
            // journal, plan, pin, inspection, or reason custody.
            RetainedEntryPoll {
                child_reaped,
                resolved: false,
            }
        }
        RetainedRegistryEnvelope::Legacy(execution) => {
            match poll_retained_execution(shared, execution, shutdown, &mut entry.signal_authorized)
            {
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

fn poll_retained_execution(
    _shared: &Shared,
    execution: &mut RetainedExecution,
    _shutdown: bool,
    signal_authorized: &mut bool,
) -> RetainedChildPoll {
    #[cfg(test)]
    let resources_present = matches!(execution, RetainedExecution::OwnedUnbound { .. });
    match execution {
        RetainedExecution::Managed { lifecycle, .. } => {
            if *signal_authorized {
                *signal_authorized = false;
                let _ = lifecycle
                    .terminate_local_cli(Instant::now() + RETAINED_LOCAL_CLI_TERMINATE_BOUND);
            }
            let state = lifecycle.state();
            #[cfg(test)]
            record_managed_observation(_shared, state.as_ref().ok());
            match state {
                Ok(ChildLifecycleState::Exited(_)) => RetainedChildPoll::Reaped,
                Ok(
                    ChildLifecycleState::Prepared
                    | ChildLifecycleState::Running
                    | ChildLifecycleState::Failed,
                )
                | Err(_) => RetainedChildPoll::Pending,
            }
        }
        RetainedExecution::Unbound(child) | RetainedExecution::OwnedUnbound { child, .. } => {
            if *signal_authorized {
                let _ = kill_unbound_observed(_shared, child);
                *signal_authorized = false;
            }
            let poll = poll_unbound(child);
            #[cfg(test)]
            record_unbound_observation(_shared, &poll, resources_present);
            match poll.outcome {
                UnboundPollOutcome::Reaped => RetainedChildPoll::Reaped,
                UnboundPollOutcome::Pending => RetainedChildPoll::Pending,
                UnboundPollOutcome::UnknownError { .. } => {
                    *signal_authorized = false;
                    RetainedChildPoll::UnknownError
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
struct UnboundPoll {
    #[cfg_attr(not(test), allow(dead_code))]
    pid: Option<u32>,
    outcome: UnboundPollOutcome,
}

#[derive(Clone, Copy)]
enum UnboundPollOutcome {
    Pending,
    Reaped,
    UnknownError {
        #[cfg_attr(not(test), allow(dead_code))]
        errno: Option<i32>,
    },
}

fn poll_unbound(unbound: &mut UnboundStartedCli) -> UnboundPoll {
    let (pid, outcome) = match &mut unbound.child {
        SpawnedCreateCli::Pty(child) => {
            (child.process_id(), classify_unbound_poll(child.try_wait()))
        }
        SpawnedCreateCli::Piped(child) | SpawnedCreateCli::PersistentPiped(child) => {
            (Some(child.id()), classify_unbound_poll(child.try_wait()))
        }
    };
    UnboundPoll { pid, outcome }
}

fn classify_unbound_poll<T>(result: std::io::Result<Option<T>>) -> UnboundPollOutcome {
    match result {
        Ok(Some(_)) => UnboundPollOutcome::Reaped,
        Ok(None) => UnboundPollOutcome::Pending,
        Err(error) => UnboundPollOutcome::UnknownError {
            errno: error.raw_os_error(),
        },
    }
}

#[cfg(test)]
fn record_unbound_observation(shared: &Shared, poll: &UnboundPoll, resources_present: bool) {
    let mut observation = shared.observation.lock().unwrap_or_else(|poisoned| {
        shared.supervisor_failed.store(true, Ordering::Release);
        poisoned.into_inner()
    });
    observation.unbound_attempts = observation.unbound_attempts.saturating_add(1);
    observation.last_pid = poll.pid;
    observation.last_resources_present = resources_present;
    observation.last_errno = None;
    match poll.outcome {
        UnboundPollOutcome::Pending => {
            observation.unbound_pending = observation.unbound_pending.saturating_add(1)
        }
        UnboundPollOutcome::Reaped => {
            observation.unbound_reaped = observation.unbound_reaped.saturating_add(1)
        }
        UnboundPollOutcome::UnknownError { errno } => {
            observation.unbound_errors = observation.unbound_errors.saturating_add(1);
            observation.last_errno = errno;
        }
    }
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

fn kill_unbound(unbound: &mut UnboundStartedCli) -> Result<(), ()> {
    match &mut unbound.child {
        SpawnedCreateCli::Pty(child) => child.kill().map_err(|_| ()),
        SpawnedCreateCli::Piped(child) | SpawnedCreateCli::PersistentPiped(child) => {
            child.kill().map_err(|_| ())
        }
    }
}

fn kill_unbound_observed(_shared: &Shared, unbound: &mut UnboundStartedCli) -> Result<(), ()> {
    #[cfg(test)]
    {
        let mut observation = _shared.observation.lock().unwrap_or_else(|poisoned| {
            _shared.supervisor_failed.store(true, Ordering::Release);
            poisoned.into_inner()
        });
        observation.unbound_kill_calls = observation.unbound_kill_calls.saturating_add(1);
    }
    kill_unbound(unbound)
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    #[derive(Clone)]
    pub(crate) struct RetentionProbe {
        shared: Arc<Shared>,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub(crate) struct RetentionSnapshot {
        pub accepting: bool,
        pub supervisor_failed: bool,
        pub retained: usize,
        pub unreaped: usize,
        pub shutdown_requested: bool,
        pub worker_detached: bool,
        pub worker_finished: bool,
        pub unbound_attempts: usize,
        pub unbound_pending: usize,
        pub unbound_errors: usize,
        pub unbound_reaped: usize,
        pub unbound_kill_calls: usize,
        pub managed_prepared: usize,
        pub managed_running: usize,
        pub managed_failed: usize,
        pub managed_exited: usize,
        pub last_pid: Option<u32>,
        pub last_errno: Option<i32>,
        pub last_resources_present: bool,
    }

    pub(crate) fn registry_thread_start_failure(
    ) -> Result<Arc<LaunchRetentionRegistry>, LaunchRetentionInitError> {
        LaunchRetentionRegistry::new_inner(true)
    }

    pub(crate) fn registry_probe(registry: &LaunchRetentionRegistry) -> RetentionProbe {
        RetentionProbe {
            shared: Arc::clone(&registry.shared),
        }
    }

    impl RetentionProbe {
        pub(crate) fn contains(&self, ticket: LaunchRetentionTicket) -> bool {
            self.shared
                .tickets
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .contains(&ticket)
        }

        pub(crate) fn only_ticket(&self) -> Option<LaunchRetentionTicket> {
            let tickets = self
                .shared
                .tickets
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if tickets.len() == 1 {
                tickets.iter().next().copied()
            } else {
                None
            }
        }

        pub(crate) fn only_termination_authorized(&self) -> Option<bool> {
            let entries = self
                .shared
                .entries
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if entries.len() == 1 {
                entries.values().next().map(|entry| entry.signal_authorized)
            } else {
                None
            }
        }

        pub(crate) fn poison_entries(&self) {
            let shared = Arc::clone(&self.shared);
            let _ = std::thread::spawn(move || {
                let _guard = shared.entries.lock().unwrap_or_else(|p| p.into_inner());
                panic!("poison launch retention entries");
            })
            .join();
        }

        pub(crate) fn snapshot(&self) -> RetentionSnapshot {
            let observation = self.shared.observation.lock().unwrap_or_else(|poisoned| {
                self.shared.supervisor_failed.store(true, Ordering::Release);
                poisoned.into_inner()
            });
            let retained = observation.retained;
            let unreaped = observation.unreaped;
            RetentionSnapshot {
                accepting: self.shared.accepting.load(Ordering::Acquire),
                supervisor_failed: self.shared.supervisor_failed.load(Ordering::Acquire),
                retained,
                unreaped,
                shutdown_requested: self.shared.shutdown_requested.load(Ordering::Acquire),
                worker_detached: self.shared.worker_detached.load(Ordering::Acquire),
                worker_finished: self.shared.worker_finished.load(Ordering::Acquire),
                unbound_attempts: observation.unbound_attempts,
                unbound_pending: observation.unbound_pending,
                unbound_errors: observation.unbound_errors,
                unbound_reaped: observation.unbound_reaped,
                unbound_kill_calls: observation.unbound_kill_calls,
                managed_prepared: observation.managed_prepared,
                managed_running: observation.managed_running,
                managed_failed: observation.managed_failed,
                managed_exited: observation.managed_exited,
                last_pid: observation.last_pid,
                last_errno: observation.last_errno,
                last_resources_present: observation.last_resources_present,
            }
        }
    }

    pub(crate) fn drop_last_owner_on_worker(
        registry: Arc<LaunchRetentionRegistry>,
        deadline: Instant,
    ) -> Result<RetentionProbe, EngineError> {
        if Arc::strong_count(&registry) != 1 {
            return Err(EngineError::Container(
                "launch retention drop requires sole owner".into(),
            ));
        }
        let probe = registry_probe(&registry);
        let shared = Arc::clone(&registry.shared);
        *shared
            .drop_on_worker
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(registry);
        shared.changed.notify_all();
        while Instant::now() < deadline {
            if probe.snapshot().shutdown_requested {
                return Ok(probe);
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        Err(EngineError::Container(
            "launch retention worker drop deadline exceeded".into(),
        ))
    }
}
