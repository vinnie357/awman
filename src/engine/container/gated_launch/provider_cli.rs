use super::ProviderCallDeadline;
use std::collections::{hash_map::Entry, HashMap};
use std::fmt;
use std::io::Read;
use std::process::{Child, ChildStderr, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::{mpsc, Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use uuid::Uuid;

#[allow(dead_code)] // Packet 1B seam; the later native-provider packet is its runtime consumer.
pub(crate) const MAX_PROVIDER_STDOUT: usize = 256 * 1024;
#[allow(dead_code)] // Packet 1B seam; the later native-provider packet is its runtime consumer.
pub(crate) const MAX_PROVIDER_STDERR: usize = 64 * 1024;
#[allow(dead_code)] // Packet 1B seam; the later native-provider packet is its runtime consumer.
pub(crate) const MAX_PROVIDER_CUSTODY_SHUTDOWN: Duration = Duration::from_secs(2);
#[allow(dead_code)] // Packet 1B seam; the later native-provider packet is its runtime consumer.
const PROVIDER_FAILURE_CLEANUP_GRACE: Duration = Duration::from_millis(250);

#[allow(dead_code)] // Packet 1B seam; the later native-provider packet is its runtime consumer.
pub(crate) struct BoundedProviderOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl fmt::Debug for BoundedProviderOutput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BoundedProviderOutput")
            .field("status", &self.status)
            .field("stdout_bytes", &self.stdout.len())
            .field("stderr_bytes", &self.stderr.len())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)] // Packet 1B seam; the later native-provider packet is its runtime consumer.
pub(crate) enum ProviderCliStartFailure {
    DeadlineExpired,
    ResourceUnavailable,
    ExecutableUnavailable,
    SpawnFailed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)] // Packet 1B seam; the later native-provider packet is its runtime consumer.
pub(crate) enum ProviderCliReapedFailureKind {
    DeadlineExceeded,
    StdoutLimitExceeded,
    StderrLimitExceeded,
    ReadFailed,
}

#[derive(Clone, Debug)]
#[allow(dead_code)] // Packet 1B seam; the later native-provider packet is its runtime consumer.
pub(crate) struct ProviderCliReapedFailure {
    pub kind: ProviderCliReapedFailureKind,
    pub status: ExitStatus,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[allow(dead_code)] // Packet 1B seam; the later native-provider packet is its runtime consumer.
pub(crate) struct ProviderCliCustodyTicket(Uuid);

#[allow(dead_code)] // Packet 1B seam; the later native-provider packet is its runtime consumer.
enum RegistryEntry {
    Reserved,
    Custody(Arc<ActorControl>),
}

#[allow(dead_code)] // Packet 1B seam; the later native-provider packet is its runtime consumer.
enum RegistryCommand {
    Shutdown {
        entries: HashMap<Uuid, RegistryEntry>,
        deadline: Instant,
    },
}

#[allow(dead_code)] // Packet 1B seam; the later native-provider packet is its runtime consumer.
pub(crate) struct ProviderCliCustodyRegistry {
    entries: Mutex<HashMap<Uuid, RegistryEntry>>,
    shutdown_tx: mpsc::Sender<RegistryCommand>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

#[allow(dead_code)] // Packet 1B seam; the later native-provider packet is its runtime consumer.
impl ProviderCliCustodyRegistry {
    pub(crate) fn try_new() -> Result<Arc<Self>, ProviderCliStartFailure> {
        let (shutdown_tx, shutdown_rx) = mpsc::channel();
        let registry = Arc::new(Self {
            entries: Mutex::new(HashMap::new()),
            shutdown_tx,
            worker: Mutex::new(None),
        });
        let worker = thread::Builder::new()
            .spawn(move || registry_worker(shutdown_rx))
            .map_err(|_| ProviderCliStartFailure::ResourceUnavailable)?;
        *lock_unpoisoned(&registry.worker) = Some(worker);
        Ok(registry)
    }

    fn reserve_ticket(&self) -> Result<ProviderCliCustodyTicket, ProviderCliStartFailure> {
        let mut entries = lock_unpoisoned(&self.entries);
        entries
            .try_reserve(1)
            .map_err(|_| ProviderCliStartFailure::ResourceUnavailable)?;
        loop {
            let ticket = Uuid::new_v4();
            match entries.entry(ticket) {
                Entry::Vacant(entry) => {
                    entry.insert(RegistryEntry::Reserved);
                    return Ok(ProviderCliCustodyTicket(ticket));
                }
                Entry::Occupied(_) => {}
            }
        }
    }

    fn cancel_ticket(&self, ticket: ProviderCliCustodyTicket) {
        let mut entries = lock_unpoisoned(&self.entries);
        if matches!(entries.get(&ticket.0), Some(RegistryEntry::Reserved)) {
            entries.remove(&ticket.0);
        }
    }

    fn install(&self, ticket: ProviderCliCustodyTicket, actor: Arc<ActorControl>) {
        let mut entries = lock_unpoisoned(&self.entries);
        if let Some(entry @ RegistryEntry::Reserved) = entries.get_mut(&ticket.0) {
            *entry = RegistryEntry::Custody(actor);
        } else {
            actor.request_terminate();
        }
    }
}

impl Drop for ProviderCliCustodyRegistry {
    fn drop(&mut self) {
        let deadline = Instant::now() + MAX_PROVIDER_CUSTODY_SHUTDOWN;
        let entries = {
            let mut held = lock_unpoisoned(&self.entries);
            std::mem::take(&mut *held)
        };
        for entry in entries.values() {
            if let RegistryEntry::Custody(actor) = entry {
                actor.request_terminate();
            }
        }
        let command = RegistryCommand::Shutdown { entries, deadline };
        if let Err(error) = self.shutdown_tx.send(command) {
            drop(error.0);
        }

        let worker = lock_unpoisoned(&self.worker).take();
        if let Some(worker) = worker {
            if worker.thread().id() == thread::current().id() {
                drop(worker);
                return;
            }
            while !worker.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(2));
            }
            if worker.is_finished() {
                let _ = worker.join();
            }
        }
    }
}

#[allow(dead_code)] // Packet 1B seam; the later native-provider packet is its runtime consumer.
fn registry_worker(receiver: mpsc::Receiver<RegistryCommand>) {
    if let Ok(RegistryCommand::Shutdown { entries, deadline }) = receiver.recv() {
        for entry in entries.into_values() {
            if let RegistryEntry::Custody(actor) = entry {
                actor.request_terminate();
                let _ = actor.wait_for_retained_terminal(deadline);
            }
            if Instant::now() >= deadline {
                break;
            }
        }
    }
}

#[must_use = "a provider CLI invocation remains owned recovery state"]
#[allow(dead_code)] // Packet 1B seam; the later native-provider packet is its runtime consumer.
pub(crate) struct RetainedProviderCli {
    actor: Arc<ActorControl>,
    registry: Arc<ProviderCliCustodyRegistry>,
    ticket: ProviderCliCustodyTicket,
    disarmed: bool,
}

#[must_use]
#[allow(dead_code)] // Packet 1B seam; the later native-provider packet is its runtime consumer.
pub(crate) enum RetainedProviderCliTermination {
    Reaped(ProviderCliReapedFailure),
    NotStarted(ProviderCliStartFailure),
    Retained(RetainedProviderCli),
}

#[allow(dead_code)] // Packet 1B seam; the later native-provider packet is its runtime consumer.
impl RetainedProviderCli {
    pub(crate) fn retry_terminate(
        mut self,
        deadline: ProviderCallDeadline,
    ) -> RetainedProviderCliTermination {
        self.actor.request_terminate();
        let bounded = std::cmp::min(
            deadline.instant(),
            Instant::now() + MAX_PROVIDER_CUSTODY_SHUTDOWN,
        );
        match self.actor.wait_for_retained_terminal(bounded) {
            Some(RetainedTerminal::Reaped(failure)) => {
                self.registry.cancel_ticket(self.ticket);
                self.disarmed = true;
                RetainedProviderCliTermination::Reaped(failure)
            }
            Some(RetainedTerminal::NotStarted(failure)) => {
                self.registry.cancel_ticket(self.ticket);
                self.disarmed = true;
                RetainedProviderCliTermination::NotStarted(failure)
            }
            None => RetainedProviderCliTermination::Retained(self),
        }
    }

    pub(crate) fn transfer(mut self) -> ProviderCliCustodyTicket {
        self.registry.install(self.ticket, Arc::clone(&self.actor));
        self.disarmed = true;
        self.ticket
    }
}

impl Drop for RetainedProviderCli {
    fn drop(&mut self) {
        if self.disarmed {
            return;
        }
        self.actor.request_terminate();
        let deadline = Instant::now() + MAX_PROVIDER_CUSTODY_SHUTDOWN;
        let _ = self.actor.wait_for_retained_terminal(deadline);
        self.registry.cancel_ticket(self.ticket);
    }
}

#[must_use]
#[allow(dead_code)] // Packet 1B seam; the later native-provider packet is its runtime consumer.
pub(crate) enum ProviderCliRunOutcome {
    Completed(BoundedProviderOutput),
    NotStarted(ProviderCliStartFailure),
    ReapedFailure(ProviderCliReapedFailure),
    RetainedFailure(RetainedProviderCli),
}

#[allow(dead_code)] // Packet 1B seam; the later native-provider packet is its runtime consumer.
enum DrainResult {
    Eof(Vec<u8>),
    LimitExceeded,
    ReadFailed,
}

#[allow(dead_code)] // Packet 1B seam; the later native-provider packet is its runtime consumer.
struct ActorStart {
    command: Command,
    deadline: Instant,
}

#[allow(dead_code)] // Packet 1B seam; the later native-provider packet is its runtime consumer.
struct StartSlot {
    state: Mutex<StartState>,
    changed: Condvar,
}

#[allow(dead_code)] // Packet 1B seam; the later native-provider packet is its runtime consumer.
struct StartState {
    start: Option<ActorStart>,
    canceled: bool,
}

#[allow(dead_code)] // Packet 1B seam; the later native-provider packet is its runtime consumer.
enum ActorInitialResult {
    Completed(BoundedProviderOutput),
    Reaped(ProviderCliReapedFailure),
    NotStarted(ProviderCliStartFailure),
}

#[allow(dead_code)] // Packet 1B seam; the later native-provider packet is its runtime consumer.
enum RetainedTerminal {
    Reaped(ProviderCliReapedFailure),
    NotStarted(ProviderCliStartFailure),
}

#[allow(dead_code)] // Packet 1B seam; the later native-provider packet is its runtime consumer.
struct ActorShared {
    initial: Option<ActorInitialResult>,
    retained_announced: bool,
    retained_reaped: Option<ProviderCliReapedFailure>,
    retained_not_started: Option<ProviderCliStartFailure>,
    termination_generation: u64,
}

#[allow(dead_code)] // Packet 1B seam; the later native-provider packet is its runtime consumer.
struct ActorControl {
    shared: Mutex<ActorShared>,
    changed: Condvar,
}

#[allow(dead_code)] // Packet 1B seam; the later native-provider packet is its runtime consumer.
impl ActorControl {
    fn new() -> Self {
        Self {
            shared: Mutex::new(ActorShared {
                initial: None,
                retained_announced: false,
                retained_reaped: None,
                retained_not_started: None,
                termination_generation: 0,
            }),
            changed: Condvar::new(),
        }
    }

    fn publish_initial(&self, result: ActorInitialResult) {
        let mut shared = lock_unpoisoned(&self.shared);
        if shared.retained_announced {
            match result {
                ActorInitialResult::Completed(output) => {
                    shared.retained_reaped = Some(ProviderCliReapedFailure {
                        kind: ProviderCliReapedFailureKind::DeadlineExceeded,
                        status: output.status,
                    });
                }
                ActorInitialResult::Reaped(failure) => {
                    shared.retained_reaped = Some(failure);
                }
                ActorInitialResult::NotStarted(failure) => {
                    shared.retained_not_started = Some(failure);
                }
            }
        } else {
            shared.initial = Some(result);
        }
        self.changed.notify_all();
    }

    fn publish_reaped(&self, failure: ProviderCliReapedFailure) {
        let mut shared = lock_unpoisoned(&self.shared);
        if shared.retained_announced {
            shared.retained_reaped = Some(failure);
        } else {
            shared.initial = Some(ActorInitialResult::Reaped(failure));
        }
        self.changed.notify_all();
    }

    fn publish_not_started(&self, failure: ProviderCliStartFailure) {
        let mut shared = lock_unpoisoned(&self.shared);
        if shared.retained_announced {
            shared.retained_not_started = Some(failure);
        } else {
            shared.initial = Some(ActorInitialResult::NotStarted(failure));
        }
        self.changed.notify_all();
    }

    fn announce_retained(&self) {
        let mut shared = lock_unpoisoned(&self.shared);
        shared.retained_announced = true;
        self.changed.notify_all();
    }

    fn request_terminate(&self) {
        let mut shared = lock_unpoisoned(&self.shared);
        shared.termination_generation = shared.termination_generation.wrapping_add(1);
        self.changed.notify_all();
    }

    fn termination_generation(&self) -> u64 {
        lock_unpoisoned(&self.shared).termination_generation
    }

    fn wait_for_initial(&self, deadline: Instant) -> Option<ActorInitialResult> {
        let mut shared = lock_unpoisoned(&self.shared);
        loop {
            if let Some(result) = shared.initial.take() {
                return Some(result);
            }
            if shared.retained_announced {
                return None;
            }
            let now = Instant::now();
            if now >= deadline {
                shared.termination_generation = shared.termination_generation.wrapping_add(1);
                shared.retained_announced = true;
                self.changed.notify_all();
                return None;
            }
            let waited = self.changed.wait_timeout(shared, deadline - now);
            shared = match waited {
                Ok((guard, _)) => guard,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
    }

    fn wait_for_retained_terminal(&self, deadline: Instant) -> Option<RetainedTerminal> {
        let mut shared = lock_unpoisoned(&self.shared);
        loop {
            if let Some(failure) = shared.retained_reaped.take() {
                return Some(RetainedTerminal::Reaped(failure));
            }
            if let Some(failure) = shared.retained_not_started.take() {
                return Some(RetainedTerminal::NotStarted(failure));
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            let waited = self.changed.wait_timeout(shared, deadline - now);
            shared = match waited {
                Ok((guard, _)) => guard,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
    }
}

#[allow(dead_code)] // Packet 1B seam; the later native-provider packet is its runtime consumer.
pub(crate) fn run_bounded_provider_cli(
    mut command: Command,
    deadline: ProviderCallDeadline,
    custody: &Arc<ProviderCliCustodyRegistry>,
) -> ProviderCliRunOutcome {
    if Instant::now() >= deadline.instant() {
        return ProviderCliRunOutcome::NotStarted(ProviderCliStartFailure::DeadlineExpired);
    }

    let stdout_buffer = match bounded_buffer(MAX_PROVIDER_STDOUT) {
        Some(buffer) => buffer,
        None => {
            return ProviderCliRunOutcome::NotStarted(ProviderCliStartFailure::ResourceUnavailable);
        }
    };
    let stderr_buffer = match bounded_buffer(MAX_PROVIDER_STDERR) {
        Some(buffer) => buffer,
        None => {
            return ProviderCliRunOutcome::NotStarted(ProviderCliStartFailure::ResourceUnavailable);
        }
    };
    let ticket = match custody.reserve_ticket() {
        Ok(ticket) => ticket,
        Err(failure) => return ProviderCliRunOutcome::NotStarted(failure),
    };

    let prepared = match PreparedCustody::new(stdout_buffer, stderr_buffer) {
        Ok(prepared) => prepared,
        Err(failure) => {
            custody.cancel_ticket(ticket);
            return ProviderCliRunOutcome::NotStarted(failure);
        }
    };

    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if Instant::now() >= deadline.instant() {
        prepared.cancel();
        custody.cancel_ticket(ticket);
        return ProviderCliRunOutcome::NotStarted(ProviderCliStartFailure::DeadlineExpired);
    }
    let actor = Arc::clone(&prepared.actor);
    prepared.start(command, deadline.instant());

    match actor.wait_for_initial(deadline.instant()) {
        Some(ActorInitialResult::Completed(output)) => {
            custody.cancel_ticket(ticket);
            ProviderCliRunOutcome::Completed(output)
        }
        Some(ActorInitialResult::Reaped(failure)) => {
            custody.cancel_ticket(ticket);
            ProviderCliRunOutcome::ReapedFailure(failure)
        }
        Some(ActorInitialResult::NotStarted(failure)) => {
            custody.cancel_ticket(ticket);
            ProviderCliRunOutcome::NotStarted(failure)
        }
        None => {
            actor.request_terminate();
            let cleanup_deadline = Instant::now() + PROVIDER_FAILURE_CLEANUP_GRACE;
            match actor.wait_for_retained_terminal(cleanup_deadline) {
                Some(RetainedTerminal::Reaped(failure)) => {
                    custody.cancel_ticket(ticket);
                    ProviderCliRunOutcome::ReapedFailure(failure)
                }
                Some(RetainedTerminal::NotStarted(failure)) => {
                    custody.cancel_ticket(ticket);
                    ProviderCliRunOutcome::NotStarted(failure)
                }
                None => ProviderCliRunOutcome::RetainedFailure(RetainedProviderCli {
                    actor,
                    registry: Arc::clone(custody),
                    ticket,
                    disarmed: false,
                }),
            }
        }
    }
}

#[allow(dead_code)] // Packet 1B seam; the later native-provider packet is its runtime consumer.
struct PreparedCustody {
    actor: Arc<ActorControl>,
    start_slot: Arc<StartSlot>,
}

#[allow(dead_code)] // Packet 1B seam; the later native-provider packet is its runtime consumer.
struct PreparedActorIo {
    stdout_work: mpsc::SyncSender<(ChildStdout, Vec<u8>)>,
    stderr_work: mpsc::SyncSender<(ChildStderr, Vec<u8>)>,
    stdout_buffer: Vec<u8>,
    stderr_buffer: Vec<u8>,
    stdout_receiver: mpsc::Receiver<DrainResult>,
    stderr_receiver: mpsc::Receiver<DrainResult>,
}

#[allow(dead_code)] // Packet 1B seam; the later native-provider packet is its runtime consumer.
impl PreparedCustody {
    fn new(
        stdout_buffer: Vec<u8>,
        stderr_buffer: Vec<u8>,
    ) -> Result<Self, ProviderCliStartFailure> {
        let (stdout_work, stdout_receiver) = mpsc::sync_channel(1);
        let (stdout_result_tx, stdout_result_rx) = mpsc::sync_channel(1);
        thread::Builder::new()
            .spawn(move || drain_stdout(stdout_receiver, stdout_result_tx))
            .map_err(|_| ProviderCliStartFailure::ResourceUnavailable)?;

        let (stderr_work, stderr_receiver) = mpsc::sync_channel(1);
        let (stderr_result_tx, stderr_result_rx) = mpsc::sync_channel(1);
        thread::Builder::new()
            .spawn(move || drain_stderr(stderr_receiver, stderr_result_tx))
            .map_err(|_| ProviderCliStartFailure::ResourceUnavailable)?;

        let actor = Arc::new(ActorControl::new());
        let start_slot = Arc::new(StartSlot {
            state: Mutex::new(StartState {
                start: None,
                canceled: false,
            }),
            changed: Condvar::new(),
        });
        let actor_for_thread = Arc::clone(&actor);
        let slot_for_thread = Arc::clone(&start_slot);
        thread::Builder::new()
            .spawn(move || {
                custody_actor(
                    slot_for_thread,
                    actor_for_thread,
                    PreparedActorIo {
                        stdout_work,
                        stderr_work,
                        stdout_buffer,
                        stderr_buffer,
                        stdout_receiver: stdout_result_rx,
                        stderr_receiver: stderr_result_rx,
                    },
                )
            })
            .map_err(|_| ProviderCliStartFailure::ResourceUnavailable)?;

        Ok(Self { actor, start_slot })
    }

    fn cancel(self) {
        let mut state = lock_unpoisoned(&self.start_slot.state);
        state.canceled = true;
        self.start_slot.changed.notify_all();
    }

    fn start(self, command: Command, deadline: Instant) {
        let mut state = lock_unpoisoned(&self.start_slot.state);
        state.start = Some(ActorStart { command, deadline });
        self.start_slot.changed.notify_all();
    }
}

#[allow(dead_code)] // Packet 1B seam; the later native-provider packet is its runtime consumer.
fn bounded_buffer(limit: usize) -> Option<Vec<u8>> {
    let mut buffer = Vec::new();
    buffer.try_reserve_exact(limit + 1).ok()?;
    Some(buffer)
}

#[allow(dead_code)] // Packet 1B seam; the later native-provider packet is its runtime consumer.
fn drain_stdout(
    receiver: mpsc::Receiver<(ChildStdout, Vec<u8>)>,
    result: mpsc::SyncSender<DrainResult>,
) {
    if let Ok((pipe, buffer)) = receiver.recv() {
        let _ = result.send(drain_pipe(pipe, buffer, MAX_PROVIDER_STDOUT));
    }
}

#[allow(dead_code)] // Packet 1B seam; the later native-provider packet is its runtime consumer.
fn drain_stderr(
    receiver: mpsc::Receiver<(ChildStderr, Vec<u8>)>,
    result: mpsc::SyncSender<DrainResult>,
) {
    if let Ok((pipe, buffer)) = receiver.recv() {
        let _ = result.send(drain_pipe(pipe, buffer, MAX_PROVIDER_STDERR));
    }
}

#[allow(dead_code)] // Packet 1B seam; the later native-provider packet is its runtime consumer.
fn drain_pipe(mut pipe: impl Read, mut buffer: Vec<u8>, limit: usize) -> DrainResult {
    let mut chunk = [0u8; 8192];
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) => return DrainResult::Eof(buffer),
            Ok(count) if count > limit.saturating_sub(buffer.len()) => {
                return DrainResult::LimitExceeded;
            }
            Ok(count) => buffer.extend_from_slice(&chunk[..count]),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return DrainResult::ReadFailed,
        }
    }
}

#[allow(dead_code)] // Packet 1B seam; the later native-provider packet is its runtime consumer.
fn custody_actor(start_slot: Arc<StartSlot>, actor: Arc<ActorControl>, io: PreparedActorIo) {
    let start = {
        let mut state = lock_unpoisoned(&start_slot.state);
        loop {
            if let Some(start) = state.start.take() {
                break start;
            }
            if state.canceled {
                return;
            }
            state = match start_slot.changed.wait(state) {
                Ok(state) => state,
                Err(poisoned) => poisoned.into_inner(),
            };
        }
    };
    if Instant::now() >= start.deadline {
        actor.publish_not_started(ProviderCliStartFailure::DeadlineExpired);
        return;
    }
    let mut command = start.command;
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            actor.publish_not_started(spawn_failure(&error));
            return;
        }
    };
    let stdout_ready = match child.stdout.take() {
        Some(stdout) => io.stdout_work.send((stdout, io.stdout_buffer)).is_ok(),
        None => false,
    };
    let stderr_ready = match child.stderr.take() {
        Some(stderr) => io.stderr_work.send((stderr, io.stderr_buffer)).is_ok(),
        None => false,
    };
    supervise_child(
        child,
        start.deadline,
        actor,
        io.stdout_receiver,
        io.stderr_receiver,
        if stdout_ready && stderr_ready {
            None
        } else {
            Some(ProviderCliReapedFailureKind::ReadFailed)
        },
    );
}

#[allow(dead_code)] // Packet 1B seam; the later native-provider packet is its runtime consumer.
fn spawn_failure(error: &std::io::Error) -> ProviderCliStartFailure {
    if error.kind() == std::io::ErrorKind::NotFound {
        ProviderCliStartFailure::ExecutableUnavailable
    } else {
        ProviderCliStartFailure::SpawnFailed
    }
}

#[allow(dead_code)] // Packet 1B seam; the later native-provider packet is its runtime consumer.
fn supervise_child(
    mut child: Child,
    deadline: Instant,
    actor: Arc<ActorControl>,
    stdout_receiver: mpsc::Receiver<DrainResult>,
    stderr_receiver: mpsc::Receiver<DrainResult>,
    initial_failure: Option<ProviderCliReapedFailureKind>,
) {
    let mut status = None;
    let mut stdout = None;
    let mut stderr = None;
    let mut failure = initial_failure;
    let mut retained = false;
    let mut observed_termination = actor.termination_generation();
    let mut kill_attempts: u8 = 0;
    let mut kill_cycle_active = false;
    let mut next_kill = Instant::now();

    loop {
        if stdout.is_none() && failure.is_none() {
            match stdout_receiver.try_recv() {
                Ok(DrainResult::Eof(bytes)) => stdout = Some(bytes),
                Ok(DrainResult::LimitExceeded) => {
                    failure = Some(ProviderCliReapedFailureKind::StdoutLimitExceeded)
                }
                Ok(DrainResult::ReadFailed) | Err(mpsc::TryRecvError::Disconnected) => {
                    failure = Some(ProviderCliReapedFailureKind::ReadFailed)
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if stderr.is_none() && failure.is_none() {
            match stderr_receiver.try_recv() {
                Ok(DrainResult::Eof(bytes)) => stderr = Some(bytes),
                Ok(DrainResult::LimitExceeded) => {
                    failure = Some(ProviderCliReapedFailureKind::StderrLimitExceeded)
                }
                Ok(DrainResult::ReadFailed) | Err(mpsc::TryRecvError::Disconnected) => {
                    failure = Some(ProviderCliReapedFailureKind::ReadFailed)
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }

        if status.is_none() {
            match child.try_wait() {
                Ok(actual) => status = actual,
                Err(_) => failure = Some(ProviderCliReapedFailureKind::ReadFailed),
            }
        }

        let now = Instant::now();
        if failure.is_none() && now >= deadline {
            failure = Some(ProviderCliReapedFailureKind::DeadlineExceeded);
        }
        let requested_termination = actor.termination_generation();
        if requested_termination != observed_termination {
            observed_termination = requested_termination;
            kill_attempts = 0;
            kill_cycle_active = true;
            next_kill = now;
        }
        if failure.is_none() && kill_cycle_active {
            failure = Some(ProviderCliReapedFailureKind::DeadlineExceeded);
        }
        if failure.is_some() && kill_attempts == 0 && !kill_cycle_active {
            kill_cycle_active = true;
            next_kill = now;
        }

        if let Some(kind) = failure {
            if status.is_none() && kill_cycle_active && Instant::now() >= next_kill {
                let _ = child.kill();
                kill_attempts += 1;
                next_kill = Instant::now()
                    + match kill_attempts {
                        1 => Duration::from_millis(10),
                        2 => Duration::from_millis(25),
                        _ => Duration::ZERO,
                    };
                if kill_attempts >= 3 {
                    kill_cycle_active = false;
                }
                if let Ok(actual) = child.try_wait() {
                    status = actual;
                }
            }
            if let Some(status) = status {
                actor.publish_reaped(ProviderCliReapedFailure { kind, status });
                return;
            }
            if !retained {
                retained = true;
                actor.announce_retained();
            }
        } else if status.is_some() && stdout.is_some() && stderr.is_some() {
            if let (Some(status), Some(stdout), Some(stderr)) =
                (status.take(), stdout.take(), stderr.take())
            {
                actor.publish_initial(ActorInitialResult::Completed(BoundedProviderOutput {
                    status,
                    stdout,
                    stderr,
                }));
                return;
            }
        }

        thread::sleep(Duration::from_millis(2));
    }
}

#[allow(dead_code)] // Packet 1B seam; the later native-provider packet is its runtime consumer.
fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}
