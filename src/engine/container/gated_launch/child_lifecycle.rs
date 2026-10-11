use std::any::Any;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU8, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use crate::engine::agent_runtime::execution::{AgentExecution, AgentExitInfo};
use crate::engine::error::EngineError;

const ACTOR_POLL_INTERVAL: Duration = Duration::from_millis(10);

pub(crate) enum SpawnedCreateCli {
    Pty(Box<dyn portable_pty::Child + Send + Sync>),
    Piped(std::process::Child),
    PersistentPiped(std::process::Child),
}

impl fmt::Debug for SpawnedCreateCli {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Pty(_) => "SpawnedCreateCli::Pty([owned])",
            Self::Piped(_) => "SpawnedCreateCli::Piped([owned])",
            Self::PersistentPiped(_) => "SpawnedCreateCli::PersistentPiped([owned])",
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ChildLifecycleState {
    Prepared,
    Running,
    Exited(AgentExitInfo),
    Failed,
}

#[derive(Debug)]
struct LifecycleState {
    state: ChildLifecycleState,
    native_reaps: usize,
    owns_unreaped_child: bool,
    #[cfg(test)]
    native_poll_attempts: usize,
    #[cfg(test)]
    native_poll_pending: usize,
    #[cfg(test)]
    native_poll_errors: usize,
    #[cfg(test)]
    native_poll_reaped: usize,
    #[cfg(test)]
    last_poll_errno: Option<i32>,
    #[cfg(test)]
    bound_pid: Option<u32>,
    #[cfg(test)]
    terminate_calls: usize,
    #[cfg(test)]
    terminate_commands: usize,
    #[cfg(test)]
    terminate_enqueued: usize,
    #[cfg(test)]
    barrier_commands: usize,
    #[cfg(test)]
    actor_finished: bool,
    #[cfg(test)]
    pause: Option<Arc<PollPauseState>>,
    #[cfg(test)]
    before_command_pause: Option<Arc<PollPauseState>>,
}

struct LifecycleShared {
    state: Mutex<LifecycleState>,
    changed: Condvar,
    actor_thread: Mutex<Option<std::thread::ThreadId>>,
    signal_authorized: AtomicBool,
    custody: Arc<ResourceCustody>,
}

struct ResourceCustody {
    slot: Mutex<Option<Box<dyn Any + Send>>>,
    actual_reaped: AtomicBool,
    loan_issued: AtomicBool,
    loan_released: AtomicBool,
    actor_finished: AtomicBool,
    may_have_child: AtomicBool,
    bind_decided: AtomicBool,
    resources_present: AtomicBool,
    raw_job: OnceLock<Mutex<RawCustodyJob>>,
    raw_ready: AtomicBool,
    raw_had_resources: AtomicBool,
    raw_kill_requested: AtomicBool,
    raw_kill_issued: AtomicBool,
    raw_outcome: AtomicU8,
    raw_pid: AtomicU32,
    raw_pid_present: AtomicBool,
    raw_errno: AtomicI32,
    raw_errno_present: AtomicBool,
    raw_attempts: AtomicUsize,
    raw_pending: AtomicUsize,
    raw_errors: AtomicUsize,
    raw_reaped: AtomicUsize,
    changed: Condvar,
    changed_state: Mutex<()>,
    #[cfg(test)]
    resource_retain_calls: AtomicUsize,
    #[cfg(test)]
    resource_handoffs_in_flight: AtomicUsize,
}

struct RawCustodyJob {
    child: UnboundStartedCliOwner,
    resources: Option<Box<dyn Any + Send>>,
}

struct UnboundStartedCliOwner {
    child: SpawnedCreateCli,
    started_at: DateTime<Utc>,
}

// `raw_job` has one unique producer (`RawHandoffPermit`) and one consumer
// (the prelaunch custody worker). The non-clonable permit is created beside a
// fresh OnceLock before launch, so its sole get_or_init closure necessarily
// installs the complete job without allocation. The producer publishes
// `raw_ready` only afterward; the worker calls get only after an Acquire
// load and is the sole locker/mutator for the rest of the job's lifetime.

const RAW_PENDING: u8 = 0;
const RAW_REAPED: u8 = 1;
const RAW_UNKNOWN: u8 = 2;

enum ActorCommand {
    Terminate {
        result: mpsc::SyncSender<TerminateResult>,
    },
    #[cfg(test)]
    Barrier { observed: mpsc::SyncSender<()> },
}

struct BindCommand {
    child: SpawnedCreateCli,
    started_at: DateTime<Utc>,
}

enum TerminateResult {
    Accepted,
    Failed,
    StateUnknown,
}

pub(super) enum TerminateRequestOutcome {
    Exited(AgentExitInfo),
    AcceptedPending(EngineError),
    NotAccepted(EngineError),
}

pub(crate) struct ChildLifecycleAuthority {
    commands: mpsc::SyncSender<ActorCommand>,
    shared: Arc<LifecycleShared>,
}

impl Clone for ChildLifecycleAuthority {
    fn clone(&self) -> Self {
        Self {
            commands: self.commands.clone(),
            shared: Arc::clone(&self.shared),
        }
    }
}

pub(crate) struct ExecutionResourceLoan {
    custody: Arc<ResourceCustody>,
    released: bool,
}

impl Drop for ExecutionResourceLoan {
    fn drop(&mut self) {
        self.release();
    }
}

impl ExecutionResourceLoan {
    fn release(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        #[cfg(test)]
        {
            self.custody
                .resource_retain_calls
                .fetch_add(1, Ordering::AcqRel);
        }
        self.custody.loan_released.store(true, Ordering::Release);
        self.custody.changed.notify_all();
    }

    pub(in crate::engine::container) fn release_and_wait(mut self) {
        self.release();
        let mut changed = self
            .custody
            .changed_state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while self.custody.resources_present.load(Ordering::Acquire) {
            changed = self
                .custody
                .changed
                .wait(changed)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }
}

impl fmt::Debug for ChildLifecycleAuthority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ChildLifecycleAuthority([redacted])")
    }
}

pub(crate) struct PreparedChildLifecycle {
    authority: ChildLifecycleAuthority,
    bind: mpsc::SyncSender<BindCommand>,
    #[cfg(test)]
    bind_fault: Option<test_support::PrepareFault>,
}

impl fmt::Debug for PreparedChildLifecycle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PreparedChildLifecycle([redacted])")
    }
}

pub(crate) struct UnboundStartedCli {
    pub(crate) child: SpawnedCreateCli,
    // Retained with raw-child custody for exact future lifecycle accounting.
    #[allow(dead_code)]
    pub(crate) started_at: DateTime<Utc>,
    raw_permit: RawHandoffPermit,
}

impl fmt::Debug for UnboundStartedCli {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UnboundStartedCli")
            .field("child", &self.child)
            .field("started_at", &self.started_at)
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub(super) struct RawCustodyCapability {
    custody: Arc<ResourceCustody>,
}

struct RawHandoffPermit {
    custody: Arc<ResourceCustody>,
}

#[derive(Clone, Copy)]
pub(super) struct RawCustodySnapshot {
    #[cfg_attr(not(test), allow(dead_code))]
    pub pid: Option<u32>,
    pub outcome: RawCustodyOutcome,
    #[cfg_attr(not(test), allow(dead_code))]
    pub resources_present: bool,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum RawCustodyOutcome {
    Pending,
    Reaped,
    UnknownError { errno: Option<i32> },
}

pub(crate) struct BindStartedChildError {
    unbound: UnboundStartedCli,
    resources: Option<Box<dyn Any + Send>>,
}

impl fmt::Debug for BindStartedChildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("BindStartedChildError([owned child])")
    }
}

impl BindStartedChildError {
    pub(in crate::engine::container) fn into_retained(self) -> RetainedExecution {
        match self.resources {
            Some(resources) => RetainedExecution::OwnedUnbound {
                child: self.unbound,
                resources,
            },
            None => RetainedExecution::Unbound(self.unbound),
        }
    }
}

pub(crate) enum RetainedExecution {
    // Frozen Packet 1 retained-execution shape.
    #[allow(dead_code)]
    Unbound(UnboundStartedCli),
    OwnedUnbound {
        child: UnboundStartedCli,
        resources: Box<dyn Any + Send>,
    },
    Managed {
        execution: Option<AgentExecution>,
        lifecycle: ChildLifecycleAuthority,
    },
}

impl fmt::Debug for RetainedExecution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unbound(_) => "RetainedExecution::Unbound([owned child])",
            Self::OwnedUnbound { .. } => {
                "RetainedExecution::OwnedUnbound([owned child and resources])"
            }
            Self::Managed {
                execution: Some(_), ..
            } => "RetainedExecution::Managed([owned execution])",
            Self::Managed {
                execution: None, ..
            } => "RetainedExecution::Managed([owned lifecycle])",
        })
    }
}

pub(crate) enum SpawnStageError {
    BeforeCliStart(EngineError),
    AfterCliStart {
        source: EngineError,
        owned: RetainedExecution,
    },
}

impl fmt::Debug for SpawnStageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BeforeCliStart(source) => f.debug_tuple("BeforeCliStart").field(source).finish(),
            Self::AfterCliStart { source, .. } => f
                .debug_struct("AfterCliStart")
                .field("source", source)
                .field("owned", &"[redacted custody]")
                .finish(),
        }
    }
}

pub(crate) fn retain_detached_after_error(owned: RetainedExecution) {
    match owned {
        RetainedExecution::Managed {
            execution,
            lifecycle,
        } => {
            drop(execution);
            drop(lifecycle);
        }
        RetainedExecution::Unbound(child) => {
            child.handoff_raw(None).request_kill();
        }
        RetainedExecution::OwnedUnbound { child, resources } => {
            child.handoff_raw(Some(resources)).request_kill();
        }
    }
}

impl UnboundStartedCli {
    pub(super) fn handoff_raw(
        self,
        resources: Option<Box<dyn Any + Send>>,
    ) -> RawCustodyCapability {
        let Self {
            child,
            started_at,
            raw_permit,
        } = self;
        let custody = raw_permit.custody;
        let had_resources = resources.is_some();
        let job = RawCustodyJob {
            child: UnboundStartedCliOwner { child, started_at },
            resources,
        };
        custody.raw_job.get_or_init(|| Mutex::new(job));
        custody
            .resources_present
            .store(had_resources, Ordering::Release);
        custody
            .raw_had_resources
            .store(had_resources, Ordering::Release);
        custody.raw_ready.store(true, Ordering::Release);
        custody.changed.notify_all();
        RawCustodyCapability { custody }
    }
}

impl RawCustodyCapability {
    pub(super) fn request_kill(&self) -> bool {
        let first = !self.custody.raw_kill_requested.swap(true, Ordering::AcqRel);
        self.custody.changed.notify_all();
        first
    }

    pub(super) fn snapshot(&self) -> RawCustodySnapshot {
        let outcome = match self.custody.raw_outcome.load(Ordering::Acquire) {
            RAW_REAPED => RawCustodyOutcome::Reaped,
            RAW_UNKNOWN => RawCustodyOutcome::UnknownError {
                errno: self
                    .custody
                    .raw_errno_present
                    .load(Ordering::Acquire)
                    .then(|| self.custody.raw_errno.load(Ordering::Acquire)),
            },
            _ => RawCustodyOutcome::Pending,
        };
        RawCustodySnapshot {
            pid: self
                .custody
                .raw_pid_present
                .load(Ordering::Acquire)
                .then(|| self.custody.raw_pid.load(Ordering::Acquire)),
            outcome,
            // Preserve the frozen registry-observation meaning: whether the
            // handed-off unbound value carried a resource bundle. Lifecycle
            // cleanup uses `ResourceCustody::resources_present`, which flips
            // only after that bundle is actually dropped.
            resources_present: self.custody.raw_had_resources.load(Ordering::Acquire),
        }
    }

    #[cfg(test)]
    pub(super) fn counters(&self) -> (usize, usize, usize, usize) {
        (
            self.custody.raw_attempts.load(Ordering::Acquire),
            self.custody.raw_pending.load(Ordering::Acquire),
            self.custody.raw_errors.load(Ordering::Acquire),
            self.custody.raw_reaped.load(Ordering::Acquire),
        )
    }
}

impl ChildLifecycleAuthority {
    pub(crate) fn prepare() -> Result<PreparedChildLifecycle, EngineError> {
        prepare_inner(
            #[cfg(test)]
            None,
            #[cfg(test)]
            None,
        )
    }

    pub(crate) fn state(&self) -> Result<ChildLifecycleState, EngineError> {
        let guard = lock_state(&self.shared);
        Ok(guard.state.clone())
    }

    pub(super) fn retained_child_is_actually_reaped(&self) -> bool {
        matches!(
            &lock_state(&self.shared).state,
            ChildLifecycleState::Exited(_)
        )
    }

    pub(crate) fn wait_actual(&self) -> Result<AgentExitInfo, EngineError> {
        let mut guard = lock_state(&self.shared);
        loop {
            match &guard.state {
                ChildLifecycleState::Exited(info) => return Ok(info.clone()),
                ChildLifecycleState::Failed => guard = wait_state(&self.shared, guard),
                _ => guard = wait_state(&self.shared, guard),
            }
        }
    }

    pub(crate) fn wait_actual_until(
        &self,
        deadline: Instant,
    ) -> Result<Option<AgentExitInfo>, EngineError> {
        let mut guard = lock_state(&self.shared);
        loop {
            match &guard.state {
                ChildLifecycleState::Exited(info) => return Ok(Some(info.clone())),
                ChildLifecycleState::Failed => return Err(state_unknown()),
                _ => {}
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Ok(None);
            };
            if remaining.is_zero() {
                return Ok(None);
            }
            guard = wait_state_timeout(&self.shared, guard, remaining);
        }
    }

    pub(crate) fn terminate_local_cli(
        &self,
        deadline: Instant,
    ) -> Result<AgentExitInfo, EngineError> {
        match self.request_terminate_local_cli(deadline) {
            TerminateRequestOutcome::Exited(info) => Ok(info),
            TerminateRequestOutcome::AcceptedPending(error)
            | TerminateRequestOutcome::NotAccepted(error) => Err(error),
        }
    }

    pub(super) fn request_terminate_local_cli(&self, deadline: Instant) -> TerminateRequestOutcome {
        #[cfg(test)]
        record_terminate_call(&self.shared);
        if Instant::now() >= deadline {
            return TerminateRequestOutcome::NotAccepted(EngineError::Container(
                "create CLI deadline exceeded".into(),
            ));
        }
        match self.state() {
            Ok(ChildLifecycleState::Exited(info)) => {
                return TerminateRequestOutcome::Exited(info);
            }
            Ok(ChildLifecycleState::Running) => {}
            Ok(ChildLifecycleState::Failed | ChildLifecycleState::Prepared) | Err(_) => {
                return TerminateRequestOutcome::NotAccepted(state_unknown());
            }
        }
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        if Instant::now() >= deadline {
            return TerminateRequestOutcome::NotAccepted(EngineError::Container(
                "create CLI deadline exceeded".into(),
            ));
        }
        if let Err(error) = self
            .commands
            .try_send(ActorCommand::Terminate { result: result_tx })
        {
            return TerminateRequestOutcome::NotAccepted(match error {
                mpsc::TrySendError::Full(_) => {
                    EngineError::Container("create CLI deadline exceeded".into())
                }
                mpsc::TrySendError::Disconnected(_) => state_unknown(),
            });
        }
        #[cfg(test)]
        record_terminate_enqueued(&self.shared);
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return TerminateRequestOutcome::AcceptedPending(EngineError::Container(
                "create CLI deadline exceeded".into(),
            ));
        };
        match result_rx.recv_timeout(remaining) {
            Ok(TerminateResult::Accepted) => {}
            Ok(TerminateResult::Failed) => {
                return TerminateRequestOutcome::AcceptedPending(EngineError::Container(
                    "create CLI termination failed".into(),
                ));
            }
            Ok(TerminateResult::StateUnknown) => {
                return TerminateRequestOutcome::AcceptedPending(state_unknown());
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return TerminateRequestOutcome::AcceptedPending(EngineError::Container(
                    "create CLI deadline exceeded".into(),
                ));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return TerminateRequestOutcome::AcceptedPending(state_unknown());
            }
        }
        match self.wait_actual_until(deadline) {
            Ok(Some(info)) => TerminateRequestOutcome::Exited(info),
            Ok(None) => TerminateRequestOutcome::AcceptedPending(EngineError::Container(
                "create CLI deadline exceeded".into(),
            )),
            Err(error) => TerminateRequestOutcome::AcceptedPending(error),
        }
    }

    pub(in crate::engine::container) fn transform_resources<T, R>(
        &self,
        transform: impl FnOnce(&mut T) -> Result<R, EngineError>,
    ) -> Result<(R, ExecutionResourceLoan), EngineError>
    where
        T: Any + Send,
    {
        let mut slot = try_lock_resource_slot(&self.shared.custody)?;
        if self.shared.custody.loan_issued.load(Ordering::Acquire) {
            return Err(state_unknown());
        }
        let resources = slot
            .as_mut()
            .and_then(|resources| resources.downcast_mut::<T>())
            .ok_or_else(state_unknown)?;
        let result = transform(resources)?;
        // The slot guard serializes every transformation. Publish the unique
        // execution loan only after the transformation succeeds, so lock
        // contention, a type mismatch, or a typed transform failure leaves
        // the still-owned bundle eligible for the retained cleanup path.
        self.shared
            .custody
            .loan_issued
            .store(true, Ordering::Release);
        Ok((
            result,
            ExecutionResourceLoan {
                custody: Arc::clone(&self.shared.custody),
                released: false,
            },
        ))
    }

    pub(in crate::engine::container) fn with_execution_resources<T, R>(
        &self,
        access: impl FnOnce(&T) -> R,
    ) -> Result<R, EngineError>
    where
        T: Any + Send,
    {
        let slot = try_lock_resource_slot(&self.shared.custody)?;
        let resources = slot
            .as_ref()
            .and_then(|resources| resources.downcast_ref::<T>())
            .ok_or_else(state_unknown)?;
        Ok(access(resources))
    }

    pub(in crate::engine::container) fn resources_are_released(&self) -> bool {
        !self
            .shared
            .custody
            .resources_present
            .load(Ordering::Acquire)
    }

    pub(super) fn execution_loan_was_issued(&self) -> bool {
        self.shared.custody.loan_issued.load(Ordering::Acquire)
    }
}

impl Drop for PreparedChildLifecycle {
    fn drop(&mut self) {
        self.authority
            .shared
            .custody
            .bind_decided
            .store(true, Ordering::Release);
        self.authority.shared.custody.changed.notify_all();
    }
}

impl PreparedChildLifecycle {
    // Packet 1 compatibility method; production uses the resource-owning sibling.
    #[allow(dead_code)]
    pub(crate) fn bind_started_child(
        self,
        child: SpawnedCreateCli,
        started_at: DateTime<Utc>,
    ) -> Result<ChildLifecycleAuthority, BindStartedChildError> {
        self.bind_started_child_with_resources(child, started_at, Box::new(()))
    }

    pub(in crate::engine::container) fn bind_started_child_with_resources(
        self,
        child: SpawnedCreateCli,
        started_at: DateTime<Utc>,
        resources: Box<dyn Any + Send>,
    ) -> Result<ChildLifecycleAuthority, BindStartedChildError> {
        let authority = self.authority.clone();
        let custody = &authority.shared.custody;
        custody.may_have_child.store(true, Ordering::Release);
        let mut slot = match try_lock_resource_slot(custody) {
            Ok(slot) => slot,
            Err(_) => {
                return Err(BindStartedChildError {
                    unbound: UnboundStartedCli {
                        child,
                        started_at,
                        raw_permit: RawHandoffPermit {
                            custody: Arc::clone(custody),
                        },
                    },
                    resources: Some(resources),
                });
            }
        };
        if slot.is_some() {
            return Err(BindStartedChildError {
                unbound: UnboundStartedCli {
                    child,
                    started_at,
                    raw_permit: RawHandoffPermit {
                        custody: Arc::clone(custody),
                    },
                },
                resources: Some(resources),
            });
        }
        *slot = Some(resources);
        custody.resources_present.store(true, Ordering::Release);
        custody.changed.notify_all();

        #[cfg(test)]
        if matches!(
            self.bind_fault,
            Some(
                test_support::PrepareFault::BindDisconnected | test_support::PrepareFault::BindFull
            )
        ) {
            let resources = slot.take();
            custody.resources_present.store(false, Ordering::Release);
            return Err(BindStartedChildError {
                unbound: UnboundStartedCli {
                    child,
                    started_at,
                    raw_permit: RawHandoffPermit {
                        custody: Arc::clone(custody),
                    },
                },
                resources,
            });
        }
        match self.bind.try_send(BindCommand { child, started_at }) {
            Ok(()) => {
                drop(slot);
                Ok(authority)
            }
            Err(
                mpsc::TrySendError::Full(BindCommand { child, started_at })
                | mpsc::TrySendError::Disconnected(BindCommand { child, started_at }),
            ) => {
                let resources = slot.take();
                custody.resources_present.store(false, Ordering::Release);
                Err(BindStartedChildError {
                    unbound: UnboundStartedCli {
                        child,
                        started_at,
                        raw_permit: RawHandoffPermit {
                            custody: Arc::clone(custody),
                        },
                    },
                    resources,
                })
            }
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct ChildLifecycleSlot(OnceLock<ChildLifecycleAuthority>);

impl ChildLifecycleSlot {
    pub(crate) fn new() -> Self {
        Self(OnceLock::new())
    }

    pub(crate) fn bind(
        &self,
        authority: ChildLifecycleAuthority,
    ) -> Result<(), ChildLifecycleAuthority> {
        self.0.set(authority)
    }

    pub(crate) fn authority(&self) -> Option<ChildLifecycleAuthority> {
        self.0.get().cloned()
    }
}

fn prepare_inner(
    #[cfg(test)] fault: Option<test_support::PrepareFault>,
    #[cfg(test)] initial_pause: Option<test_support::ActorPausePoint>,
) -> Result<PreparedChildLifecycle, EngineError> {
    #[cfg(test)]
    if fault == Some(test_support::PrepareFault::ThreadStart) {
        return Err(EngineError::Container(
            "create CLI actor unavailable".into(),
        ));
    }
    let (commands, receiver) = mpsc::sync_channel(1);
    let (bind, bind_receiver) = mpsc::sync_channel(1);
    let custody = Arc::new(ResourceCustody {
        slot: Mutex::new(None),
        actual_reaped: AtomicBool::new(false),
        loan_issued: AtomicBool::new(false),
        loan_released: AtomicBool::new(false),
        actor_finished: AtomicBool::new(false),
        may_have_child: AtomicBool::new(false),
        bind_decided: AtomicBool::new(false),
        resources_present: AtomicBool::new(false),
        raw_job: OnceLock::new(),
        raw_ready: AtomicBool::new(false),
        raw_had_resources: AtomicBool::new(false),
        raw_kill_requested: AtomicBool::new(false),
        raw_kill_issued: AtomicBool::new(false),
        raw_outcome: AtomicU8::new(RAW_PENDING),
        raw_pid: AtomicU32::new(0),
        raw_pid_present: AtomicBool::new(false),
        raw_errno: AtomicI32::new(0),
        raw_errno_present: AtomicBool::new(false),
        raw_attempts: AtomicUsize::new(0),
        raw_pending: AtomicUsize::new(0),
        raw_errors: AtomicUsize::new(0),
        raw_reaped: AtomicUsize::new(0),
        changed: Condvar::new(),
        changed_state: Mutex::new(()),
        #[cfg(test)]
        resource_retain_calls: AtomicUsize::new(0),
        #[cfg(test)]
        resource_handoffs_in_flight: AtomicUsize::new(0),
    });
    let worker_custody = Arc::clone(&custody);
    let custody_worker = std::thread::Builder::new()
        .name("awman-create-cli-resources".into())
        .spawn(move || resource_custody_loop(worker_custody))
        .map_err(|_| EngineError::Container("create CLI resource custody unavailable".into()))?;
    drop(custody_worker);
    let shared = Arc::new(LifecycleShared {
        state: Mutex::new(LifecycleState {
            state: ChildLifecycleState::Prepared,
            native_reaps: 0,
            owns_unreaped_child: false,
            #[cfg(test)]
            native_poll_attempts: 0,
            #[cfg(test)]
            native_poll_pending: 0,
            #[cfg(test)]
            native_poll_errors: 0,
            #[cfg(test)]
            native_poll_reaped: 0,
            #[cfg(test)]
            last_poll_errno: None,
            #[cfg(test)]
            bound_pid: None,
            #[cfg(test)]
            terminate_calls: 0,
            #[cfg(test)]
            terminate_commands: 0,
            #[cfg(test)]
            terminate_enqueued: 0,
            #[cfg(test)]
            barrier_commands: 0,
            #[cfg(test)]
            actor_finished: false,
            #[cfg(test)]
            pause: initial_pause
                .filter(|point| *point == test_support::ActorPausePoint::BeforePoll)
                .map(|_| new_pause_state()),
            #[cfg(test)]
            before_command_pause: initial_pause
                .filter(|point| *point == test_support::ActorPausePoint::BeforeFirstCommand)
                .map(|_| new_pause_state()),
        }),
        changed: Condvar::new(),
        actor_thread: Mutex::new(None),
        signal_authorized: AtomicBool::new(true),
        custody: Arc::clone(&custody),
    });
    let actor_shared = Arc::clone(&shared);
    let actor = match std::thread::Builder::new()
        .name("awman-create-cli".into())
        .spawn(move || actor_loop(bind_receiver, receiver, actor_shared))
    {
        Ok(actor) => actor,
        Err(_) => {
            custody.bind_decided.store(true, Ordering::Release);
            custody.changed.notify_all();
            return Err(EngineError::Container(
                "create CLI actor unavailable".into(),
            ));
        }
    };
    *shared
        .actor_thread
        .lock()
        .unwrap_or_else(|p| p.into_inner()) = Some(actor.thread().id());
    drop(actor);
    Ok(PreparedChildLifecycle {
        authority: ChildLifecycleAuthority { commands, shared },
        bind,
        #[cfg(test)]
        bind_fault: fault,
    })
}

fn actor_loop(
    bind_receiver: mpsc::Receiver<BindCommand>,
    receiver: mpsc::Receiver<ActorCommand>,
    shared: Arc<LifecycleShared>,
) {
    if let Ok(mut actor_thread) = shared.actor_thread.lock() {
        *actor_thread = Some(std::thread::current().id());
    }
    #[cfg(test)]
    pause_before_command(&shared);
    let BindCommand {
        child: bound,
        started_at,
    } = match bind_receiver.recv() {
        Ok(bound) => bound,
        Err(_) => {
            record_actor_finished(&shared);
            return;
        }
    };
    #[cfg(test)]
    record_bound_custody(&shared, process_id(&bound), true);
    let mut child = Some(bound);
    update_state(&shared, ChildLifecycleState::Running, true);
    let mut disconnected = false;
    loop {
        let command = if disconnected {
            std::thread::sleep(ACTOR_POLL_INTERVAL);
            Err(mpsc::RecvTimeoutError::Disconnected)
        } else {
            receiver.recv_timeout(ACTOR_POLL_INTERVAL)
        };
        match command {
            Ok(ActorCommand::Terminate { result }) => {
                #[cfg(test)]
                record_terminate_command(&shared);
                if shared.signal_authorized.load(Ordering::Acquire) {
                    if let Some(owned) = child.as_mut() {
                        if native_kill(owned).is_err() {
                            let poll = native_try_wait(owned, Some(started_at));
                            #[cfg(test)]
                            record_native_poll(&shared, &poll);
                            match poll {
                                Ok(Some(info)) => {
                                    publish_exit(&shared, info);
                                    child = None;
                                    let _ = result.try_send(TerminateResult::Accepted);
                                }
                                Ok(None) => {
                                    let _ = result.try_send(TerminateResult::Failed);
                                }
                                Err(_) => {
                                    shared.signal_authorized.store(false, Ordering::Release);
                                    update_state(&shared, ChildLifecycleState::Failed, true);
                                    let _ = result.try_send(TerminateResult::StateUnknown);
                                }
                            }
                        } else {
                            let _ = result.try_send(TerminateResult::Accepted);
                        }
                    } else {
                        let _ = result.try_send(TerminateResult::StateUnknown);
                    }
                } else {
                    let _ = result.try_send(TerminateResult::StateUnknown);
                }
            }
            #[cfg(test)]
            Ok(ActorCommand::Barrier { observed }) => {
                record_barrier_command(&shared);
                let _ = observed.try_send(());
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => disconnected = true,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if child.is_none() {
            if disconnected {
                break;
            }
            continue;
        }
        #[cfg(test)]
        pause_before_poll(&shared);
        let Some(owned_child) = child.as_mut() else {
            continue;
        };
        let result = native_try_wait(owned_child, Some(started_at));
        #[cfg(test)]
        record_native_poll(&shared, &result);
        match result {
            Ok(Some(info)) => {
                publish_exit(&shared, info);
                child = None;
                if disconnected {
                    break;
                }
            }
            Ok(None) => {}
            Err(_) => {
                shared.signal_authorized.store(false, Ordering::Release);
                update_state(&shared, ChildLifecycleState::Failed, true);
            }
        }
    }
    record_actor_finished(&shared);
}

fn resource_custody_loop(custody: Arc<ResourceCustody>) {
    let mut changed = custody
        .changed_state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    loop {
        if custody.raw_ready.load(Ordering::Acquire) {
            if !custody.raw_kill_requested.load(Ordering::Acquire) {
                let (next, _) = custody
                    .changed
                    .wait_timeout(changed, ACTOR_POLL_INTERVAL)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                changed = next;
                continue;
            }
            // `raw_ready` was acquired above. The unique permit completed the
            // OnceLock initialization before its Release publication, and this
            // worker is the only locker/mutator for the job's lifetime.
            let Some(job_slot) = custody.raw_job.get() else {
                custody.raw_errno_present.store(false, Ordering::Release);
                custody.raw_outcome.store(RAW_UNKNOWN, Ordering::Release);
                custody.changed.notify_all();
                let (next, _) = custody
                    .changed
                    .wait_timeout(changed, ACTOR_POLL_INTERVAL)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                changed = next;
                continue;
            };
            let mut job = job_slot
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let pid = process_id(&job.child.child);
            if let Some(pid) = pid {
                custody.raw_pid.store(pid, Ordering::Release);
                custody.raw_pid_present.store(true, Ordering::Release);
            }
            if custody.raw_kill_requested.load(Ordering::Acquire)
                && !custody.raw_kill_issued.swap(true, Ordering::AcqRel)
            {
                let _ = native_kill(&mut job.child.child);
            }
            let started_at = job.child.started_at;
            let poll = native_try_wait(&mut job.child.child, Some(started_at));
            custody.raw_attempts.fetch_add(1, Ordering::AcqRel);
            match poll {
                Ok(Some(_)) => {
                    custody.raw_reaped.fetch_add(1, Ordering::AcqRel);
                    custody.raw_errno_present.store(false, Ordering::Release);
                    let resources = job.resources.take();
                    drop(resources);
                    custody.resources_present.store(false, Ordering::Release);
                    custody.raw_outcome.store(RAW_REAPED, Ordering::Release);
                    custody.changed.notify_all();
                    return;
                }
                Ok(None) => {
                    custody.raw_pending.fetch_add(1, Ordering::AcqRel);
                    custody.raw_errno_present.store(false, Ordering::Release);
                    custody.raw_outcome.store(RAW_PENDING, Ordering::Release);
                }
                Err(error) => {
                    custody.raw_errors.fetch_add(1, Ordering::AcqRel);
                    if let Some(errno) = error.errno {
                        custody.raw_errno.store(errno, Ordering::Release);
                        custody.raw_errno_present.store(true, Ordering::Release);
                    } else {
                        custody.raw_errno_present.store(false, Ordering::Release);
                    }
                    custody.raw_outcome.store(RAW_UNKNOWN, Ordering::Release);
                }
            }
        }
        let execution_cleanup_ready = if custody.loan_issued.load(Ordering::Acquire) {
            custody.loan_released.load(Ordering::Acquire)
        } else {
            custody.actor_finished.load(Ordering::Acquire)
        };
        if custody.actual_reaped.load(Ordering::Acquire) && execution_cleanup_ready {
            if let Ok(mut slot) = try_lock_resource_slot(&custody) {
                let resources = slot.take();
                drop(slot);
                drop(resources);
                custody.resources_present.store(false, Ordering::Release);
                custody.changed.notify_all();
                return;
            }
        }
        if custody.bind_decided.load(Ordering::Acquire)
            && !custody.may_have_child.load(Ordering::Acquire)
        {
            return;
        }
        let (next, _) = custody
            .changed
            .wait_timeout(changed, ACTOR_POLL_INTERVAL)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        changed = next;
    }
}

#[derive(Debug)]
struct NativePollError {
    #[cfg_attr(not(test), allow(dead_code))]
    errno: Option<i32>,
}

fn process_id(child: &SpawnedCreateCli) -> Option<u32> {
    match child {
        SpawnedCreateCli::Pty(child) => child.process_id(),
        SpawnedCreateCli::Piped(child) | SpawnedCreateCli::PersistentPiped(child) => {
            Some(child.id())
        }
    }
}

fn native_try_wait(
    child: &mut SpawnedCreateCli,
    started_at: Option<DateTime<Utc>>,
) -> Result<Option<AgentExitInfo>, NativePollError> {
    let started_at = started_at.ok_or(NativePollError { errno: None })?;
    match child {
        SpawnedCreateCli::Pty(child) => child
            .try_wait()
            .map_err(|error| NativePollError {
                errno: error.raw_os_error(),
            })
            .map(|status| {
                status.map(|status| AgentExitInfo {
                    exit_code: status.exit_code().try_into().unwrap_or(-1),
                    signal: None,
                    started_at,
                    ended_at: Utc::now(),
                })
            }),
        SpawnedCreateCli::Piped(child) | SpawnedCreateCli::PersistentPiped(child) => child
            .try_wait()
            .map_err(|error| NativePollError {
                errno: error.raw_os_error(),
            })
            .map(|status| {
                status.map(|status| {
                    #[cfg(unix)]
                    let signal = {
                        use std::os::unix::process::ExitStatusExt;
                        status.signal()
                    };
                    #[cfg(not(unix))]
                    let signal = None;
                    AgentExitInfo {
                        exit_code: status.code().unwrap_or(-1),
                        signal,
                        started_at,
                        ended_at: Utc::now(),
                    }
                })
            }),
    }
}

#[cfg(test)]
fn record_native_poll(
    shared: &LifecycleShared,
    result: &Result<Option<AgentExitInfo>, NativePollError>,
) {
    let mut guard = lock_state(shared);
    guard.native_poll_attempts = guard.native_poll_attempts.saturating_add(1);
    match result {
        Ok(Some(_)) => {
            guard.native_poll_reaped = guard.native_poll_reaped.saturating_add(1);
            guard.last_poll_errno = None;
        }
        Ok(None) => {
            guard.native_poll_pending = guard.native_poll_pending.saturating_add(1);
            guard.last_poll_errno = None;
        }
        Err(error) => {
            guard.native_poll_errors = guard.native_poll_errors.saturating_add(1);
            guard.last_poll_errno = error.errno;
        }
    }
}

#[cfg(test)]
fn record_bound_custody(shared: &LifecycleShared, pid: Option<u32>, resources_present: bool) {
    let mut guard = lock_state(shared);
    guard.bound_pid = pid;
    shared
        .custody
        .resources_present
        .store(resources_present, Ordering::Release);
}

fn native_kill(child: &mut SpawnedCreateCli) -> Result<(), ()> {
    match child {
        SpawnedCreateCli::Pty(child) => child.kill().map_err(|_| ()),
        SpawnedCreateCli::Piped(child) | SpawnedCreateCli::PersistentPiped(child) => {
            child.kill().map_err(|_| ())
        }
    }
}

fn publish_exit(shared: &LifecycleShared, info: AgentExitInfo) {
    {
        let mut guard = lock_state(shared);
        guard.native_reaps += 1;
        guard.owns_unreaped_child = false;
        guard.state = ChildLifecycleState::Exited(info);
    }
    shared.custody.actual_reaped.store(true, Ordering::Release);
    shared.changed.notify_all();
    shared.custody.changed.notify_all();
}

fn update_state(shared: &LifecycleShared, state: ChildLifecycleState, owns: bool) {
    let mut guard = lock_state(shared);
    guard.state = if shared.signal_authorized.load(Ordering::Acquire) {
        state
    } else {
        ChildLifecycleState::Failed
    };
    guard.owns_unreaped_child = owns;
    shared.changed.notify_all();
}

fn lock_state(shared: &LifecycleShared) -> std::sync::MutexGuard<'_, LifecycleState> {
    match shared.state.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            let mut guard = poisoned.into_inner();
            if !matches!(guard.state, ChildLifecycleState::Exited(_)) {
                guard.state = ChildLifecycleState::Failed;
                guard.owns_unreaped_child = true;
                shared.signal_authorized.store(false, Ordering::Release);
            }
            shared.changed.notify_all();
            guard
        }
    }
}

fn try_lock_resource_slot(
    custody: &ResourceCustody,
) -> Result<std::sync::MutexGuard<'_, Option<Box<dyn Any + Send>>>, EngineError> {
    match custody.slot.try_lock() {
        Ok(guard) => Ok(guard),
        Err(std::sync::TryLockError::Poisoned(poisoned)) => Ok(poisoned.into_inner()),
        Err(std::sync::TryLockError::WouldBlock) => Err(state_unknown()),
    }
}

fn wait_state<'a>(
    shared: &'a LifecycleShared,
    guard: std::sync::MutexGuard<'a, LifecycleState>,
) -> std::sync::MutexGuard<'a, LifecycleState> {
    match shared.changed.wait(guard) {
        Ok(guard) => guard,
        Err(poisoned) => mark_poisoned(shared, poisoned.into_inner()),
    }
}

fn wait_state_timeout<'a>(
    shared: &'a LifecycleShared,
    guard: std::sync::MutexGuard<'a, LifecycleState>,
    timeout: Duration,
) -> std::sync::MutexGuard<'a, LifecycleState> {
    match shared.changed.wait_timeout(guard, timeout) {
        Ok((guard, _)) => guard,
        Err(poisoned) => mark_poisoned(shared, poisoned.into_inner().0),
    }
}

fn mark_poisoned<'a>(
    shared: &LifecycleShared,
    mut guard: std::sync::MutexGuard<'a, LifecycleState>,
) -> std::sync::MutexGuard<'a, LifecycleState> {
    if !matches!(guard.state, ChildLifecycleState::Exited(_)) {
        guard.state = ChildLifecycleState::Failed;
        guard.owns_unreaped_child = true;
        shared.signal_authorized.store(false, Ordering::Release);
    }
    shared.changed.notify_all();
    guard
}

fn state_unknown() -> EngineError {
    EngineError::Container("create CLI state unknown".into())
}

#[cfg(test)]
fn record_terminate_call(shared: &LifecycleShared) {
    let mut guard = lock_state(shared);
    guard.terminate_calls = guard.terminate_calls.saturating_add(1);
}

#[cfg(test)]
fn record_terminate_command(shared: &LifecycleShared) {
    let mut guard = lock_state(shared);
    guard.terminate_commands = guard.terminate_commands.saturating_add(1);
}

#[cfg(test)]
fn record_terminate_enqueued(shared: &LifecycleShared) {
    let mut guard = lock_state(shared);
    guard.terminate_enqueued = guard.terminate_enqueued.saturating_add(1);
}

#[cfg(test)]
fn record_barrier_command(shared: &LifecycleShared) {
    let mut guard = lock_state(shared);
    guard.barrier_commands = guard.barrier_commands.saturating_add(1);
}

#[cfg(test)]
fn new_pause_state() -> Arc<PollPauseState> {
    Arc::new(PollPauseState {
        flags: Mutex::new(PollPauseFlags {
            requested: true,
            actor_paused: false,
        }),
        paused: Condvar::new(),
        resume: Condvar::new(),
    })
}

#[cfg(test)]
fn pause_before_command(shared: &LifecycleShared) {
    let pause = lock_state(shared).before_command_pause.clone();
    pause_on_request(pause);
}

fn record_actor_finished(shared: &LifecycleShared) {
    shared.custody.actor_finished.store(true, Ordering::Release);
    shared.custody.changed.notify_all();
    #[cfg(test)]
    {
        let mut guard = lock_state(shared);
        guard.actor_finished = true;
        shared.changed.notify_all();
    }
}

#[cfg(test)]
#[derive(Debug)]
struct PollPauseState {
    flags: Mutex<PollPauseFlags>,
    paused: Condvar,
    resume: Condvar,
}

#[cfg(test)]
#[derive(Debug)]
struct PollPauseFlags {
    requested: bool,
    actor_paused: bool,
}

#[cfg(test)]
fn pause_before_poll(shared: &LifecycleShared) {
    let pause = lock_state(shared).pause.clone();
    pause_on_request(pause);
}

#[cfg(test)]
fn pause_on_request(pause: Option<Arc<PollPauseState>>) {
    let Some(pause) = pause else { return };
    let mut flags = pause.flags.lock().unwrap_or_else(|p| p.into_inner());
    if !flags.requested {
        return;
    }
    flags.actor_paused = true;
    pause.paused.notify_all();
    while flags.requested {
        flags = pause.resume.wait(flags).unwrap_or_else(|p| p.into_inner());
    }
    flags.actor_paused = false;
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub(crate) enum PrepareFault {
        ThreadStart,
        BindDisconnected,
        BindFull,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub(crate) enum ActorPausePoint {
        BeforeFirstCommand,
        BeforePoll,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub(crate) enum CreateCliKind {
        Pty,
        Piped,
        PersistentPiped,
    }

    pub(crate) fn prepare_with_fault(
        fault: PrepareFault,
    ) -> Result<PreparedChildLifecycle, EngineError> {
        prepare_inner(Some(fault), None)
    }

    pub(crate) fn prepare_with_pause(
        point: ActorPausePoint,
    ) -> Result<(PreparedChildLifecycle, PollPause), EngineError> {
        let prepared = prepare_inner(None, Some(point))?;
        let pause = {
            let guard = lock_state(&prepared.authority.shared);
            match point {
                ActorPausePoint::BeforeFirstCommand => guard.before_command_pause.clone(),
                ActorPausePoint::BeforePoll => guard.pause.clone(),
            }
            .expect("initial lifecycle pause installed")
        };
        Ok((prepared, PollPause { state: pause }))
    }

    pub(crate) fn unbound_kind_and_started_at(
        child: &UnboundStartedCli,
    ) -> (CreateCliKind, DateTime<Utc>) {
        let kind = match child.child {
            SpawnedCreateCli::Pty(_) => CreateCliKind::Pty,
            SpawnedCreateCli::Piped(_) => CreateCliKind::Piped,
            SpawnedCreateCli::PersistentPiped(_) => CreateCliKind::PersistentPiped,
        };
        (kind, child.started_at)
    }

    #[derive(Clone)]
    pub(crate) struct LifecycleProbe {
        shared: Arc<LifecycleShared>,
    }

    #[derive(Clone, Debug)]
    pub(crate) struct LifecycleSnapshot {
        // Frozen lifecycle inspection keeps the actor identity for later assertions.
        #[allow(dead_code)]
        pub actor_thread: std::thread::ThreadId,
        pub native_reaps: usize,
        pub actual_exit: Option<AgentExitInfo>,
        pub owns_unreaped_child: bool,
        pub state: ChildLifecycleState,
        pub native_poll_attempts: usize,
        pub native_poll_pending: usize,
        pub native_poll_errors: usize,
        pub native_poll_reaped: usize,
        pub last_poll_errno: Option<i32>,
        pub bound_pid: Option<u32>,
        pub resources_present: bool,
        pub terminate_calls: usize,
        pub terminate_commands: usize,
        pub terminate_enqueued: usize,
        pub barrier_commands: usize,
        pub resource_retain_calls: usize,
        pub resource_handoffs_in_flight: usize,
        pub actor_finished: bool,
    }

    pub(crate) struct PollPause {
        state: Arc<PollPauseState>,
    }

    impl Drop for PollPause {
        fn drop(&mut self) {
            let mut flags = self.state.flags.lock().unwrap_or_else(|p| p.into_inner());
            flags.requested = false;
            self.state.resume.notify_all();
        }
    }

    impl PollPause {
        pub(crate) fn wait_until_paused(&self, deadline: Instant) -> bool {
            let mut flags = self.state.flags.lock().unwrap_or_else(|p| p.into_inner());
            while flags.requested && !flags.actor_paused {
                let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                    return false;
                };
                let (next, timeout) = self
                    .state
                    .paused
                    .wait_timeout(flags, remaining)
                    .unwrap_or_else(|p| p.into_inner());
                flags = next;
                if timeout.timed_out() {
                    return false;
                }
            }
            flags.actor_paused
        }
    }

    impl LifecycleProbe {
        pub(crate) fn snapshot(&self) -> LifecycleSnapshot {
            let guard = lock_state(&self.shared);
            let actor_thread = self
                .shared
                .actor_thread
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .expect("actor thread recorded before lifecycle preparation returns");
            LifecycleSnapshot {
                actor_thread,
                native_reaps: guard.native_reaps,
                actual_exit: match &guard.state {
                    ChildLifecycleState::Exited(info) => Some(info.clone()),
                    _ => None,
                },
                owns_unreaped_child: guard.owns_unreaped_child,
                state: guard.state.clone(),
                native_poll_attempts: guard.native_poll_attempts,
                native_poll_pending: guard.native_poll_pending,
                native_poll_errors: guard.native_poll_errors,
                native_poll_reaped: guard.native_poll_reaped,
                last_poll_errno: guard.last_poll_errno,
                bound_pid: guard.bound_pid,
                resources_present: self
                    .shared
                    .custody
                    .resources_present
                    .load(Ordering::Acquire),
                terminate_calls: guard.terminate_calls,
                terminate_commands: guard.terminate_commands,
                terminate_enqueued: guard.terminate_enqueued,
                barrier_commands: guard.barrier_commands,
                resource_retain_calls: self
                    .shared
                    .custody
                    .resource_retain_calls
                    .load(Ordering::Acquire),
                resource_handoffs_in_flight: self
                    .shared
                    .custody
                    .resource_handoffs_in_flight
                    .load(Ordering::Acquire),
                actor_finished: guard.actor_finished,
            }
        }

        pub(crate) fn pause_before_next_poll(&self) -> PollPause {
            let state = new_pause_state();
            lock_state(&self.shared).pause = Some(Arc::clone(&state));
            PollPause { state }
        }
    }

    pub(crate) struct QueuedBarrier {
        observed: mpsc::Receiver<()>,
    }

    impl QueuedBarrier {
        pub(crate) fn wait_until_consumed(&self, deadline: Instant) -> bool {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            self.observed.recv_timeout(remaining).is_ok()
        }
    }

    pub(crate) fn queue_barrier(
        authority: &ChildLifecycleAuthority,
    ) -> Result<QueuedBarrier, EngineError> {
        let (observed_tx, observed) = mpsc::sync_channel(1);
        authority
            .commands
            .try_send(ActorCommand::Barrier {
                observed: observed_tx,
            })
            .map_err(|_| state_unknown())?;
        Ok(QueuedBarrier { observed })
    }

    pub(crate) fn lifecycle_probe(authority: &ChildLifecycleAuthority) -> LifecycleProbe {
        LifecycleProbe {
            shared: Arc::clone(&authority.shared),
        }
    }
}
