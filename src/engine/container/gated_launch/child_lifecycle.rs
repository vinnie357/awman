use std::any::Any;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
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
    resources_present: bool,
    #[cfg(test)]
    terminate_calls: usize,
    #[cfg(test)]
    terminate_commands: usize,
    #[cfg(test)]
    actor_finished: bool,
    #[cfg(test)]
    pause: Option<Arc<PollPauseState>>,
}

struct LifecycleShared {
    state: Mutex<LifecycleState>,
    changed: Condvar,
    actor_thread: Mutex<Option<std::thread::ThreadId>>,
    signal_authorized: AtomicBool,
}

enum ActorCommand {
    Bind {
        child: SpawnedCreateCli,
        started_at: DateTime<Utc>,
        resources: Box<dyn Any + Send>,
    },
    Terminate {
        result: mpsc::SyncSender<TerminateResult>,
    },
    TakeResources {
        result: mpsc::SyncSender<Option<Box<dyn Any + Send>>>,
    },
    RetainResources(Box<dyn Any + Send>),
}

enum TerminateResult {
    Accepted,
    Failed,
    StateUnknown,
}

#[derive(Clone)]
pub(crate) struct ChildLifecycleAuthority {
    commands: mpsc::SyncSender<ActorCommand>,
    shared: Arc<LifecycleShared>,
}

impl fmt::Debug for ChildLifecycleAuthority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ChildLifecycleAuthority([redacted])")
    }
}

pub(crate) struct PreparedChildLifecycle {
    authority: ChildLifecycleAuthority,
    #[cfg(test)]
    bind_fault: Option<test_support::PrepareFault>,
}

impl fmt::Debug for PreparedChildLifecycle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PreparedChildLifecycle([redacted])")
    }
}

#[derive(Debug)]
pub(crate) struct UnboundStartedCli {
    pub(crate) child: SpawnedCreateCli,
    // Retained with raw-child custody for exact future lifecycle accounting.
    #[allow(dead_code)]
    pub(crate) started_at: DateTime<Utc>,
}

pub(crate) struct BindStartedChildError {
    unbound: UnboundStartedCli,
    resources: Box<dyn Any + Send>,
}

impl fmt::Debug for BindStartedChildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("BindStartedChildError([owned child])")
    }
}

impl BindStartedChildError {
    // Packet 1 compatibility accessor; production retains the resource bundle too.
    #[allow(dead_code)]
    pub(crate) fn into_unbound(self) -> UnboundStartedCli {
        self.unbound
    }

    pub(in crate::engine::container) fn into_retained(self) -> RetainedExecution {
        RetainedExecution::OwnedUnbound {
            child: self.unbound,
            resources: self.resources,
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

impl ChildLifecycleAuthority {
    pub(crate) fn prepare() -> Result<PreparedChildLifecycle, EngineError> {
        prepare_inner(
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
        #[cfg(test)]
        record_terminate_call(&self.shared);
        if Instant::now() >= deadline {
            return Err(EngineError::Container(
                "create CLI deadline exceeded".into(),
            ));
        }
        match self.state()? {
            ChildLifecycleState::Exited(info) => return Ok(info),
            ChildLifecycleState::Failed | ChildLifecycleState::Prepared => {
                return Err(state_unknown())
            }
            ChildLifecycleState::Running => {}
        }
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        if Instant::now() >= deadline {
            return Err(EngineError::Container(
                "create CLI deadline exceeded".into(),
            ));
        }
        self.commands
            .try_send(ActorCommand::Terminate { result: result_tx })
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => {
                    EngineError::Container("create CLI deadline exceeded".into())
                }
                mpsc::TrySendError::Disconnected(_) => state_unknown(),
            })?;
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Err(EngineError::Container(
                "create CLI deadline exceeded".into(),
            ));
        };
        match result_rx.recv_timeout(remaining) {
            Ok(TerminateResult::Accepted) => {}
            Ok(TerminateResult::Failed) => {
                return Err(EngineError::Container(
                    "create CLI termination failed".into(),
                ));
            }
            Ok(TerminateResult::StateUnknown) => return Err(state_unknown()),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Err(EngineError::Container(
                    "create CLI deadline exceeded".into(),
                ));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return Err(state_unknown()),
        }
        match self.wait_actual_until(deadline)? {
            Some(info) => Ok(info),
            None => Err(EngineError::Container(
                "create CLI deadline exceeded".into(),
            )),
        }
    }

    pub(in crate::engine::container) fn take_resources<T: Any + Send>(
        &self,
    ) -> Result<T, EngineError> {
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        self.commands
            .send(ActorCommand::TakeResources { result: result_tx })
            .map_err(|_| state_unknown())?;
        let resources = result_rx.recv().map_err(|_| state_unknown())?;
        match resources.ok_or_else(state_unknown)?.downcast::<T>() {
            Ok(resources) => Ok(*resources),
            Err(resources) => {
                std::mem::forget(resources);
                Err(state_unknown())
            }
        }
    }

    pub(in crate::engine::container) fn retain_resources<T: Any + Send>(&self, resources: T) {
        let resources: Box<dyn Any + Send> = Box::new(resources);
        match self.commands.send(ActorCommand::RetainResources(resources)) {
            Ok(()) => {}
            Err(error) => std::mem::forget(error.0),
        }
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
        #[cfg(test)]
        if matches!(
            self.bind_fault,
            Some(
                test_support::PrepareFault::BindDisconnected | test_support::PrepareFault::BindFull
            )
        ) {
            return Err(BindStartedChildError {
                unbound: UnboundStartedCli { child, started_at },
                resources,
            });
        }
        self.authority
            .commands
            .try_send(ActorCommand::Bind {
                child,
                started_at,
                resources,
            })
            .map_err(|error| match error {
                mpsc::TrySendError::Full(ActorCommand::Bind {
                    child,
                    started_at,
                    resources,
                })
                | mpsc::TrySendError::Disconnected(ActorCommand::Bind {
                    child,
                    started_at,
                    resources,
                }) => BindStartedChildError {
                    unbound: UnboundStartedCli { child, started_at },
                    resources,
                },
                _ => unreachable!("bind sends only a bind command"),
            })?;
        Ok(self.authority)
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
) -> Result<PreparedChildLifecycle, EngineError> {
    #[cfg(test)]
    if fault == Some(test_support::PrepareFault::ThreadStart) {
        return Err(EngineError::Container(
            "create CLI actor unavailable".into(),
        ));
    }
    let (commands, receiver) = mpsc::sync_channel(1);
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
            resources_present: false,
            #[cfg(test)]
            terminate_calls: 0,
            #[cfg(test)]
            terminate_commands: 0,
            #[cfg(test)]
            actor_finished: false,
            #[cfg(test)]
            pause: None,
        }),
        changed: Condvar::new(),
        actor_thread: Mutex::new(None),
        signal_authorized: AtomicBool::new(true),
    });
    let actor_shared = Arc::clone(&shared);
    let actor = std::thread::Builder::new()
        .name("awman-create-cli".into())
        .spawn(move || actor_loop(receiver, actor_shared))
        .map_err(|_| EngineError::Container("create CLI actor unavailable".into()))?;
    *shared
        .actor_thread
        .lock()
        .unwrap_or_else(|p| p.into_inner()) = Some(actor.thread().id());
    drop(actor);
    Ok(PreparedChildLifecycle {
        authority: ChildLifecycleAuthority { commands, shared },
        #[cfg(test)]
        bind_fault: fault,
    })
}

fn actor_loop(receiver: mpsc::Receiver<ActorCommand>, shared: Arc<LifecycleShared>) {
    if let Ok(mut actor_thread) = shared.actor_thread.lock() {
        *actor_thread = Some(std::thread::current().id());
    }
    let mut child: Option<SpawnedCreateCli> = None;
    let mut started_at = None;
    let mut disconnected = false;
    let mut resources: Option<Box<dyn Any + Send>> = None;
    loop {
        let command = if disconnected {
            std::thread::sleep(ACTOR_POLL_INTERVAL);
            Err(mpsc::RecvTimeoutError::Disconnected)
        } else {
            receiver.recv_timeout(ACTOR_POLL_INTERVAL)
        };
        match command {
            Ok(ActorCommand::Bind {
                child: bound,
                started_at: started,
                resources: owned,
            }) => {
                #[cfg(test)]
                record_bound_custody(&shared, process_id(&bound), true);
                child = Some(bound);
                started_at = Some(started);
                resources = Some(owned);
                update_state(&shared, ChildLifecycleState::Running, true);
            }
            Ok(ActorCommand::Terminate { result }) => {
                #[cfg(test)]
                record_terminate_command(&shared);
                if shared.signal_authorized.load(Ordering::Acquire) {
                    if let Some(owned) = child.as_mut() {
                        if native_kill(owned).is_err() {
                            let poll = native_try_wait(owned, started_at);
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
            Ok(ActorCommand::TakeResources { result }) => {
                let _ = result.try_send(resources.take());
                #[cfg(test)]
                record_resources_present(&shared, resources.is_some());
            }
            Ok(ActorCommand::RetainResources(owned)) => {
                if resources.is_some() {
                    std::mem::forget(owned);
                } else {
                    resources = Some(owned);
                }
                #[cfg(test)]
                record_resources_present(&shared, resources.is_some());
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
        let result = native_try_wait(child.as_mut().expect("checked"), started_at);
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
    #[cfg(test)]
    {
        drop(resources.take());
        record_actor_finished(&shared);
    }
}

#[derive(Debug)]
struct NativePollError {
    #[cfg_attr(not(test), allow(dead_code))]
    errno: Option<i32>,
}

#[cfg(test)]
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
    guard.resources_present = resources_present;
}

#[cfg(test)]
fn record_resources_present(shared: &LifecycleShared, resources_present: bool) {
    lock_state(shared).resources_present = resources_present;
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
    let mut guard = lock_state(shared);
    guard.native_reaps += 1;
    guard.owns_unreaped_child = false;
    guard.state = ChildLifecycleState::Exited(info);
    shared.changed.notify_all();
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
fn record_actor_finished(shared: &LifecycleShared) {
    let mut guard = lock_state(shared);
    guard.resources_present = false;
    guard.actor_finished = true;
    shared.changed.notify_all();
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
    pub(crate) enum CreateCliKind {
        Pty,
        Piped,
        PersistentPiped,
    }

    pub(crate) fn prepare_with_fault(
        fault: PrepareFault,
    ) -> Result<PreparedChildLifecycle, EngineError> {
        prepare_inner(Some(fault))
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
                resources_present: guard.resources_present,
                terminate_calls: guard.terminate_calls,
                terminate_commands: guard.terminate_commands,
                actor_finished: guard.actor_finished,
            }
        }

        pub(crate) fn pause_before_next_poll(&self) -> PollPause {
            let state = Arc::new(PollPauseState {
                flags: Mutex::new(PollPauseFlags {
                    requested: true,
                    actor_paused: false,
                }),
                paused: Condvar::new(),
                resume: Condvar::new(),
            });
            lock_state(&self.shared).pause = Some(Arc::clone(&state));
            PollPause { state }
        }
    }

    pub(crate) fn lifecycle_probe(authority: &ChildLifecycleAuthority) -> LifecycleProbe {
        LifecycleProbe {
            shared: Arc::clone(&authority.shared),
        }
    }
}
