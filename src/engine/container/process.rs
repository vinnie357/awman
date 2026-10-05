//! The one container process module — `pub(super)`.
//!
//! Docker and Apple Containers are the same CLI-shaped runtime: `<bin> run`
//! with the argv `docker.rs::build_run_argv` produces, a child spawned either
//! behind a PTY or on piped stdio, and the bytes bridged to the frontend's
//! `AgentIo` through `io_bridge`. Before WI 0113 that path existed twice, once
//! per backend, differing in ~51 of ~92 lines that were all the binary name
//! (`docker` ↔ `container`) or a type name (report F-11).
//!
//! This module owns it once:
//!
//! - [`ContainerCli`] is the whole of the per-backend difference: the binary
//!   name, the ownership label, the startup-chatter delay, and a post-wait
//!   hook.
//! - [`ContainerInstance`] and [`ContainerExecution`] replace the
//!   `Docker*`/`Apple*` pairs.
//! - [`SpawnRequest`] replaces the six 7-parameter spawn signatures.
//! - [`spawn_piped`], [`spawn_piped_interactive`] and [`spawn_pty_bridged`]
//!   are the three spawn paths.
//!
//! The genuinely backend-specific behaviour survives as two hooks and nothing
//! else:
//!
//! - [`PostWaitHook`] — Docker's `clear_stdio_nonblocking`, which repairs the
//!   O_NONBLOCK flags `docker -it` leaves on the inherited stdio fds. Apple's
//!   `container` does not do that, so its hook is [`no_post_wait`].
//! - [`AttachHook`] — Apple's attach rendezvous. Apple's CLI has no `attach`
//!   verb, so the launching process serves the live PTY over a per-container
//!   unix socket. Docker passes `None` and keeps `docker attach`.
//!
//! Everything else about a backend — `list`/`stats`/`stop` parsing, image
//! inspection, `attach` — stays in `docker.rs` / `apple.rs`, where the two
//! genuinely differ.

use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use crate::data::session::AgentHandle;
use crate::engine::agent_runtime::execution::{
    AgentExecution, AgentExitInfo, AgentHandlePreview, AgentInstance, ExecutionBackend,
};
use crate::engine::agent_runtime::frontend::AgentIo;
use crate::engine::container::attach_socket::AttachSocketGuard;
use crate::engine::container::gated_launch::LaunchRetentionRegistry;
use crate::engine::container::gated_launch::{
    retain_detached_after_error, validate_immediate_pre_spawn, ChildLifecycleAuthority,
    ChildLifecycleSlot, DurableLaunchPlan, ExecutionResourceLoan, GatedProviderAdapter,
    LaunchRetentionReason, RetainedAgentLaunch, RetainedExecution, SpawnStageError,
    SpawnedCreateCli,
};
use crate::engine::container::instance::{handle_now, ContainerId};
use crate::engine::container::io_bridge::{BridgeConfig, CancelFn};
use crate::engine::container::options::{ContainerName, ImageRef, ResolvedContainerOptions};
use crate::engine::credential_refresh::CredentialLease;
use crate::engine::error::EngineError;

/// The `Arc<Mutex<…>>` the I/O bridge hands back for a PTY-bridged run.
type PtyMaster = Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>>;

// ─── Per-backend CLI description ────────────────────────────────────────────

/// Ran once after a piped child exits, before the exit status is read.
///
/// Docker's `-it` leaves O_NONBLOCK set on the inherited stdio fds; Apple's
/// `container` does not. This is the only difference between the two backends'
/// `wait_blocking` piped path.
pub(super) type PostWaitHook = fn();

/// The no-op [`PostWaitHook`] — used by every backend that leaves the process's
/// stdio the way it found it.
pub(super) fn no_post_wait() {}

/// Everything the shared spawn path needs to know about a container CLI.
///
/// A backend is a `ContainerCli` plus its own `list`/`stats`/`stop` parsing.
/// Adding a third CLI-shaped runtime should mean adding a constant here, not
/// a third copy of [`spawn_piped`].
#[derive(Clone, Copy)]
pub(super) struct ContainerCli {
    /// The binary invoked for `run`, `stop` and `rm`.
    pub bin: &'static str,
    /// Ownership label stamped on every awman-spawned container so a backend's
    /// `list_running` can filter to ours.
    pub label: &'static str,
    /// Window after launch during which the stuck detector ignores output.
    /// Apple's `container` prints its own startup chatter ("creating
    /// container…") before the workload writes a byte; Docker does not.
    pub start_delay: Duration,
    /// Ran after a piped child exits. See [`PostWaitHook`].
    pub post_wait: PostWaitHook,
}

impl std::fmt::Debug for ContainerCli {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContainerCli")
            .field("bin", &self.bin)
            .field("label", &self.label)
            .field("start_delay", &self.start_delay)
            .finish_non_exhaustive()
    }
}

impl ContainerCli {
    pub(super) const DOCKER: ContainerCli = ContainerCli {
        bin: "docker",
        label: "awman=true",
        start_delay: Duration::ZERO,
        post_wait: super::docker::clear_stdio_nonblocking,
    };

    pub(super) const APPLE: ContainerCli = ContainerCli {
        bin: "container",
        label: "awman=true",
        start_delay: crate::engine::container::timing::APPLE_CONTAINER_START_DELAY,
        post_wait: no_post_wait,
    };
}

// ─── The attach hook (Apple's attach rendezvous) ────────────────────────────

/// What a [`AttachHook`] is handed once the PTY bridge is wired up.
pub(super) struct AttachHookCtx<'a> {
    /// The container's name — the key the socket path is derived from.
    pub container_name: &'a str,
    /// Live tap on the container's output. Already installed on the
    /// `BridgeConfig`, so every chunk the reader threads forward arrives here.
    pub output_broadcast: Arc<tokio::sync::broadcast::Sender<Vec<u8>>>,
    /// The bridge's stdin injector, so an attach client's keystrokes merge
    /// with whatever the launching frontend sends.
    pub stdin_injector: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    /// A *weak* reference to the PTY master: the attach server must never keep
    /// the PTY alive past the execution backend that owns it.
    pub pty_master: Weak<Mutex<Box<dyn portable_pty::MasterPty + Send>>>,
}

/// Backend hook run after [`spawn_pty_bridged`] wires the I/O bridge.
///
/// Returning `Some` stores the guard on the [`ContainerExecution`], which drops
/// it (closing the socket) as soon as the container exits. Returning `None` —
/// which the hook does on any failure, after logging — means the container runs
/// but cannot be attached to.
pub(super) type AttachHook = fn(AttachHookCtx<'_>) -> Option<AttachSocketGuard>;

pub(super) struct GatedSpawnContext {
    pub plan: DurableLaunchPlan,
    pub adapter: Arc<dyn GatedProviderAdapter>,
    pub enclosing: Instant,
}

struct SpawnRetentionState {
    plan: Option<DurableLaunchPlan>,
    reason: Option<LaunchRetentionReason>,
}

pub(super) struct SpawnRetentionContext {
    state: Mutex<SpawnRetentionState>,
}

impl SpawnRetentionContext {
    pub(super) fn new(plan: Option<DurableLaunchPlan>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(SpawnRetentionState { plan, reason: None }),
        })
    }

    fn initialize_plan(&self, plan: Option<DurableLaunchPlan>) {
        let Some(plan) = plan else { return };
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.plan = Some(plan);
    }

    fn record_reason(&self, reason: LaunchRetentionReason) {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .reason = Some(reason);
    }

    fn take(&self) -> (Option<DurableLaunchPlan>, Option<LaunchRetentionReason>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        (state.plan.take(), state.reason.take())
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SpawnFault {
    AfterBindBeforeBridge,
}

// ─── Bridge configuration ───────────────────────────────────────────────────

/// Build a `BridgeConfig` for this container, including a cancel callback that
/// runs `<bin> stop <name>` so the startup-grace detector can kill a container
/// that never produced output. We construct the same `stop` invocation the
/// backend's `cancel_handle` would issue.
pub(super) fn bridge_config_for(
    cli: ContainerCli,
    name: &ContainerName,
    grace_timeout: Duration,
    stuck_timeout: Duration,
) -> BridgeConfig {
    let container_name = name.0.clone();
    let bin = cli.bin;
    let cancel: CancelFn = Arc::new(move || {
        let _ = Command::new(bin)
            .args(["stop", &container_name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    });
    BridgeConfig {
        grace_timeout,
        stuck_timeout,
        container_start_delay: cli.start_delay,
        cancel_on_grace_expired: Some(cancel),
        output_tail: Arc::new(
            crate::engine::agent_runtime::output_tail::OutputTail::with_default_capacity(),
        ),
        output_broadcast: None,
    }
}

// ─── The instance ───────────────────────────────────────────────────────────

/// A built-but-not-running container. Both backends' `build` produce this.
pub(super) struct ContainerInstance {
    /// Which CLI runs this container. Also the value the spawn functions are
    /// handed; they take it as an explicit parameter so a test can substitute
    /// a stand-in binary without reaching into the instance.
    pub cli: ContainerCli,
    pub id: ContainerId,
    pub name: ContainerName,
    pub image: ImageRef,
    pub options: ResolvedContainerOptions,
    /// Credential-refresh leases, one per file-delivered credential. Moved into
    /// the [`ContainerExecution`] by each spawn fn so the lease's lifetime
    /// brackets the child process exactly (this instance box is dropped before
    /// the spawn fn returns).
    pub leases: Vec<CredentialLease>,
    /// Backend hook for the PTY path. `Some` only for Apple, whose CLI has no
    /// native attach. See [`AttachHook`].
    pub post_bridge: Option<AttachHook>,
    pub gate_cleanup: Option<super::startup_gate::StartupGateCleanup>,
    pub launch_retention: Option<Arc<LaunchRetentionRegistry>>,
}

impl ContainerInstance {
    // Legacy nongated construction remains available until all backends wire retention.
    #[allow(dead_code)]
    pub(super) fn new(
        cli: ContainerCli,
        image: ImageRef,
        name: ContainerName,
        options: ResolvedContainerOptions,
        leases: Vec<CredentialLease>,
        post_bridge: Option<AttachHook>,
    ) -> Self {
        Self::new_with_launch_retention(cli, image, name, options, leases, post_bridge, None)
    }

    pub(super) fn new_with_launch_retention(
        cli: ContainerCli,
        image: ImageRef,
        name: ContainerName,
        options: ResolvedContainerOptions,
        leases: Vec<CredentialLease>,
        post_bridge: Option<AttachHook>,
        launch_retention: Option<Arc<LaunchRetentionRegistry>>,
    ) -> Self {
        Self {
            cli,
            id: ContainerId::new(name.0.clone()),
            name,
            image,
            options,
            leases,
            post_bridge,
            gate_cleanup: None,
            launch_retention,
        }
    }
}

impl AgentInstance for ContainerInstance {
    fn handle_preview(&self) -> AgentHandlePreview {
        AgentHandlePreview {
            id: self.id.0.clone(),
            name: self.name.0.clone(),
            image: self.image.0.clone(),
        }
    }

    fn run_with_frontend(
        mut self: Box<Self>,
        mut frontend: Box<dyn crate::engine::agent_runtime::frontend::AgentFrontend>,
    ) -> Result<AgentExecution, EngineError> {
        let cli = self.cli;
        let post_bridge = self.post_bridge;
        if let Some(spec) = self.options.startup_gate.clone() {
            let mut staged = super::startup_gate::stage_startup_gate(&spec)?;
            staged
                .wrapper_argv
                .extend(["--container-name".into(), self.name.0.clone()]);
            if let Some(user) = &self.options.startup_gate_runtime_user {
                staged
                    .wrapper_argv
                    .extend(["--run-as".into(), user.clone()]);
            }
            let mut python_env = std::collections::BTreeMap::new();
            for variable in &self.options.env_passthrough {
                if matches!(variable.0.as_str(), "PYTHONHOME" | "PYTHONPATH") {
                    if let Some(value) = crate::data::config::env::host_var(&variable.0) {
                        python_env.insert(variable.0.clone(), value);
                    }
                }
            }
            for literal in &self.options.env_literal {
                if matches!(literal.key.as_str(), "PYTHONHOME" | "PYTHONPATH") {
                    python_env.insert(literal.key.clone(), literal.value.clone());
                }
            }
            staged.preserve_python_environment(&python_env)?;
            self.options
                .env_passthrough
                .retain(|variable| !matches!(variable.0.as_str(), "PYTHONHOME" | "PYTHONPATH"));
            self.options
                .env_literal
                .retain(|literal| !matches!(literal.key.as_str(), "PYTHONHOME" | "PYTHONPATH"));
            let original = self
                .options
                .entrypoint
                .as_ref()
                .ok_or_else(|| {
                    EngineError::MissingRequiredOption("startup gate entrypoint".into())
                })?
                .0
                .clone();
            let wrapped = super::startup_gate::wrap_entrypoint(&staged, &original)?;
            self.options.overlays.extend(staged.overlays.clone());
            self.options.entrypoint = Some(crate::engine::container::options::Entrypoint(wrapped));
            self.gate_cleanup = Some(staged.cleanup);
        }
        let argv = super::docker::build_run_argv(&self.name, &self.image, &self.options);
        let started_at = chrono::Utc::now();
        let seeded = self.options.seeded_prompt.clone();
        let handle = handle_now(&self.id, &self.name, &self.image);

        frontend.report_status(
            crate::engine::agent_runtime::frontend::AgentStatus::Running {
                container_name: self.name.0.clone(),
            },
        );

        // Read per-frontend timeouts before draining `take_io`, which leaves
        // the frontend in a state where any further calls are
        // implementation-defined.
        let grace_timeout = frontend.grace_timeout();
        let stuck_timeout = frontend.stuck_timeout();
        let io = frontend.take_io();

        let prepared = ChildLifecycleAuthority::prepare()?;
        let slot = Arc::new(ChildLifecycleSlot::new());
        let retention = SpawnRetentionContext::new(None);
        let bridge_cfg = bridge_config_for(cli, &self.name, grace_timeout, stuck_timeout);
        let req = SpawnRequest {
            io,
            argv,
            seeded,
            started_at,
            handle,
            bridge_cfg,
            prepared: Some(prepared),
            slot: Some(slot),
            gated: None,
            retention: Arc::clone(&retention),
            #[cfg(test)]
            fault: None,
        };

        // PTY path: frontend requested interactive PTY bridging.
        if req.io.initial_size.is_some() {
            return finish_spawn(
                self.launch_retention.clone(),
                retention,
                spawn_pty_bridged(cli, self, req, post_bridge),
            );
        }

        // ACP path: persistent piped stdio (no PTY). Unlike the one-shot piped
        // path below, the stdin channel is kept open for the whole session so
        // the ACP driver can carry a full bidirectional JSON-RPC exchange.
        // Every backend takes this path — shipping ACP under only one runtime
        // would be a platform-inconsistent regression (WI 0104 Edge Case
        // Considerations).
        if self.options.acp {
            return finish_spawn(
                self.launch_retention.clone(),
                retention,
                spawn_piped_interactive(cli, self, req),
            );
        }

        // Piped path: non-interactive or no PTY.
        finish_spawn(
            self.launch_retention.clone(),
            retention,
            spawn_piped(cli, self, req),
        )
    }
}

// ─── The spawn request ──────────────────────────────────────────────────────

/// Everything a spawn function needs beyond the instance and its CLI.
///
/// Replaces the six 7-parameter signatures the two backends carried before
/// WI 0113 (report F-11).
pub(super) struct SpawnRequest {
    /// The frontend's I/O channels, drained from `take_io`.
    pub io: AgentIo,
    /// `<bin>`-less argv from `build_run_argv`.
    pub argv: Vec<String>,
    /// Prompt to write into stdin before the writer task starts. Honoured only
    /// by [`spawn_piped`]; see the other two functions for why.
    pub seeded: Option<String>,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub handle: AgentHandle,
    pub bridge_cfg: BridgeConfig,
    pub prepared: Option<super::gated_launch::PreparedChildLifecycle>,
    pub slot: Option<Arc<ChildLifecycleSlot>>,
    pub gated: Option<GatedSpawnContext>,
    pub retention: Arc<SpawnRetentionContext>,
    #[cfg(test)]
    pub fault: Option<SpawnFault>,
}

enum SpawnIoResources {
    Pty(portable_pty::PtyPair),
    Piped(crate::engine::container::io_bridge::PipedChildIo),
}

struct SpawnCustodyResources {
    io: Option<SpawnIoResources>,
    leases: Option<Vec<CredentialLease>>,
    gate_cleanup: Option<super::startup_gate::StartupGateCleanup>,
    post_wait: PostWaitHook,
    run_post_wait: bool,
    attach_store: Arc<PreallocatedAttachStore>,
    attach_permit: Option<AttachStorePermit>,
    #[cfg(test)]
    custody_witness: Option<test_support::CustodyWitness>,
}

struct SpawnExecutionParts {
    io: SpawnIoResources,
    leases: Vec<CredentialLease>,
    gate_cleanup: Option<super::startup_gate::StartupGateCleanup>,
    post_wait: PostWaitHook,
    attach_store: Arc<PreallocatedAttachStore>,
    attach_permit: AttachStorePermit,
    #[cfg(test)]
    custody_witness: Option<test_support::CustodyWitness>,
}

impl SpawnCustodyResources {
    fn take_execution_parts(&mut self) -> Option<SpawnExecutionParts> {
        if self.io.is_none() || self.leases.is_none() || self.attach_permit.is_none() {
            return None;
        }
        self.run_post_wait = false;
        Some(SpawnExecutionParts {
            io: self.io.take()?,
            leases: self.leases.take()?,
            gate_cleanup: self.gate_cleanup.take(),
            post_wait: self.post_wait,
            attach_store: Arc::clone(&self.attach_store),
            attach_permit: self.attach_permit.take()?,
            #[cfg(test)]
            custody_witness: self.custody_witness.take(),
        })
    }

    fn restore_execution_parts(&mut self, parts: SpawnExecutionParts) {
        self.io = Some(parts.io);
        self.leases = Some(parts.leases);
        self.gate_cleanup = parts.gate_cleanup;
        self.post_wait = parts.post_wait;
        self.attach_store = parts.attach_store;
        self.attach_permit = Some(parts.attach_permit);
        self.run_post_wait = true;
        #[cfg(test)]
        {
            self.custody_witness = parts.custody_witness;
        }
    }
}

impl Drop for SpawnCustodyResources {
    fn drop(&mut self) {
        if self.run_post_wait {
            (self.post_wait)();
        }
    }
}

struct ExecutionResources {
    post_wait: PostWaitHook,
    pty_master: Option<PtyMaster>,
    stdin_injector: Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
    _attach_store: Arc<PreallocatedAttachStore>,
    leases: Vec<CredentialLease>,
    gate_cleanup: Option<super::startup_gate::StartupGateCleanup>,
    #[cfg(test)]
    _custody_witness: Option<test_support::CustodyWitness>,
}

struct PreallocatedAttachStore {
    value: OnceLock<AttachSocketGuard>,
}

struct AttachStorePermit {
    store: Arc<PreallocatedAttachStore>,
}

impl PreallocatedAttachStore {
    fn pair() -> (Arc<Self>, AttachStorePermit) {
        let store = Arc::new(Self {
            value: OnceLock::new(),
        });
        let permit = AttachStorePermit {
            store: Arc::clone(&store),
        };
        (store, permit)
    }
}

impl AttachStorePermit {
    fn store(self, guard: AttachSocketGuard) {
        // The non-clonable permit is created beside a fresh OnceLock before
        // launch. Its sole get_or_init call therefore runs this non-allocating
        // initializer exactly once; Arc drop later runs the guard destructor.
        self.store.value.get_or_init(|| guard);
    }
}

struct FailedPtyBridgeResources {
    _pair: portable_pty::PtyPair,
    _reader: Option<Box<dyn std::io::Read + Send>>,
    _io: AgentIo,
    _config: BridgeConfig,
    _custody: SpawnCustodyResources,
}

enum LifecycleResources {
    Spawn(SpawnCustodyResources),
    Execution(ExecutionResources),
    #[allow(dead_code)]
    FailedPty(FailedPtyBridgeResources),
}

struct ExecutionBridgeArtifacts {
    stuck_tx: Arc<tokio::sync::broadcast::Sender<crate::engine::agent_runtime::StuckEvent>>,
    output_tail: Arc<crate::engine::agent_runtime::output_tail::OutputTail>,
    attach: Option<(AttachStorePermit, AttachHookCtxOwned)>,
}

struct AttachHookCtxOwned {
    output_broadcast: Arc<tokio::sync::broadcast::Sender<Vec<u8>>>,
    stdin_injector: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    pty_master: Weak<Mutex<Box<dyn portable_pty::MasterPty + Send>>>,
}

impl LifecycleResources {
    fn spawn(&self) -> Option<&SpawnCustodyResources> {
        match self {
            Self::Spawn(resources) => Some(resources),
            Self::Execution(_) | Self::FailedPty(_) => None,
        }
    }

    fn spawn_mut(&mut self) -> Option<&mut SpawnCustodyResources> {
        match self {
            Self::Spawn(resources) => Some(resources),
            Self::Execution(_) | Self::FailedPty(_) => None,
        }
    }

    fn execution(&self) -> Option<&ExecutionResources> {
        match self {
            Self::Execution(resources) => Some(resources),
            Self::Spawn(_) | Self::FailedPty(_) => None,
        }
    }
}

impl Drop for ExecutionResources {
    fn drop(&mut self) {
        (self.post_wait)();
        self.pty_master = None;
        drop(self.stdin_injector.take());
        self.leases.clear();
        drop(self.gate_cleanup.take());
    }
}

fn bridge_piped_resources(
    lifecycle: &ChildLifecycleAuthority,
    io: AgentIo,
    bridge_cfg: BridgeConfig,
    keep_stdin_injector: bool,
) -> Result<(ExecutionBridgeArtifacts, ExecutionResourceLoan), EngineError> {
    lifecycle.transform_resources::<LifecycleResources, _>(move |resources| {
        let LifecycleResources::Spawn(spawn) = resources else {
            return Err(EngineError::Container(
                "create CLI piped resource transformation failed".into(),
            ));
        };
        let Some(parts) = spawn.take_execution_parts() else {
            return Err(EngineError::Container(
                "create CLI piped resource transformation failed".into(),
            ));
        };
        let SpawnExecutionParts {
            io: io_resources,
            leases,
            gate_cleanup,
            post_wait,
            attach_store,
            attach_permit,
            #[cfg(test)]
            custody_witness,
        } = parts;
        let pipes = match io_resources {
            SpawnIoResources::Piped(pipes) => pipes,
            wrong_kind => {
                spawn.restore_execution_parts(SpawnExecutionParts {
                    io: wrong_kind,
                    leases,
                    gate_cleanup,
                    post_wait,
                    attach_store,
                    attach_permit,
                    #[cfg(test)]
                    custody_witness,
                });
                return Err(EngineError::Container(
                    "create CLI piped resource kind changed".into(),
                ));
            }
        };
        let bridge = crate::engine::container::io_bridge::bridge_piped_io(io, pipes, bridge_cfg);
        let crate::engine::container::io_bridge::BridgeResult {
            stdin_injector,
            stuck_tx,
            output_tail,
        } = bridge;
        let stdin_injector = keep_stdin_injector.then_some(stdin_injector);
        drop(attach_permit);
        *resources = LifecycleResources::Execution(ExecutionResources {
            post_wait,
            pty_master: None,
            stdin_injector,
            _attach_store: attach_store,
            leases,
            gate_cleanup,
            #[cfg(test)]
            _custody_witness: custody_witness,
        });
        Ok(ExecutionBridgeArtifacts {
            stuck_tx,
            output_tail,
            attach: None,
        })
    })
}

fn finish_spawn(
    registry: Option<Arc<LaunchRetentionRegistry>>,
    retention: Arc<SpawnRetentionContext>,
    result: Result<AgentExecution, SpawnStageError>,
) -> Result<AgentExecution, EngineError> {
    match result {
        Ok(execution) => Ok(execution),
        Err(SpawnStageError::BeforeCliStart(source)) => Err(source),
        Err(SpawnStageError::AfterCliStart { source, owned }) => {
            let (plan, reason) = retention.take();
            let reason = reason.unwrap_or(LaunchRetentionReason::ChildStateUnknown);
            if let Some(registry) = registry {
                if let Some(plan) = plan {
                    let _ = registry.retain(RetainedAgentLaunch {
                        plan,
                        execution: Some(owned),
                        last_inspection: None,
                        reason,
                    });
                } else {
                    let _ = registry.retain_legacy(owned);
                }
            } else {
                retain_detached_after_error(owned);
            }
            Err(source)
        }
    }
}

/// Common `<bin> <argv>` setup: the child command with the agent credentials
/// and the resolved `env()` passthrough values on its environment.
///
/// Both are passed as name-only `-e KEY` in argv; their values are set on the
/// child CLI's environment so the CLI resolves them without the secret ever
/// touching the argument vector.
///
/// The passthrough values must be injected explicitly rather than relying on
/// ambient inheritance: inside the squad daemon they come from the Layer 0
/// daemon overlay, not from the daemon's own process environment.
fn piped_command(
    cli: ContainerCli,
    argv: &[String],
    options: &ResolvedContainerOptions,
) -> Command {
    let mut cmd = Command::new(cli.bin);
    cmd.args(argv);
    for (k, v) in &options.agent_credentials {
        cmd.env(k, v);
    }
    for (k, v) in super::docker::resolve_env_passthrough(options) {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd
}

/// INV-6: a credentialed container must hold its lease before the child is
/// spawned. This closes the startup race where a token could expire between
/// staging and the container's first request.
fn assert_leases_before_spawn(options: &ResolvedContainerOptions, leases: &[CredentialLease]) {
    debug_assert!(
        options.refreshable_credentials.is_empty()
            || !leases.is_empty()
            || crate::engine::credential_refresh::global().is_none(),
        "file-delivered credential spawned without a lease"
    );
}

fn spawn_child(cli: ContainerCli, cmd: &mut Command) -> Result<std::process::Child, EngineError> {
    cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            EngineError::ContainerRuntimeUnavailable {
                binary: cli.bin.into(),
            }
        } else {
            EngineError::Container(format!("spawn {}: {e}", cli.bin))
        }
    })
}

type PreparedSpawnGate = (
    Option<DurableLaunchPlan>,
    Option<Arc<dyn GatedProviderAdapter>>,
    Option<Instant>,
);

type PreparedChild = (
    super::gated_launch::PreparedChildLifecycle,
    Arc<ChildLifecycleSlot>,
);

fn prepare_spawn_gate(gated: Option<GatedSpawnContext>) -> Result<PreparedSpawnGate, EngineError> {
    let Some(gated) = gated else {
        return Ok((None, None, None));
    };
    let barrier =
        validate_immediate_pre_spawn(&gated.plan, gated.adapter.as_ref(), gated.enclosing)
            .map_err(|error| EngineError::Container(error.to_string()))?;
    let plan = barrier
        .consume_for_spawn(gated.plan)
        .map_err(|error| EngineError::Container(error.to_string()))?;
    Ok((Some(plan), Some(gated.adapter), Some(gated.enclosing)))
}

fn before_cli_start(error: EngineError) -> SpawnStageError {
    SpawnStageError::BeforeCliStart(error)
}

fn ensure_child_lifecycle(
    prepared: Option<super::gated_launch::PreparedChildLifecycle>,
    slot: Option<Arc<ChildLifecycleSlot>>,
) -> Result<PreparedChild, EngineError> {
    let prepared = match prepared {
        Some(prepared) => prepared,
        None => ChildLifecycleAuthority::prepare()?,
    };
    let slot = slot.unwrap_or_else(|| Arc::new(ChildLifecycleSlot::new()));
    Ok((prepared, slot))
}

fn after_cli_start(
    retention: &SpawnRetentionContext,
    source: EngineError,
    owned: RetainedExecution,
    reason: LaunchRetentionReason,
) -> SpawnStageError {
    retention.record_reason(reason);
    SpawnStageError::AfterCliStart { source, owned }
}

fn validate_after_spawn(
    plan: &Option<DurableLaunchPlan>,
    _adapter: Option<&dyn GatedProviderAdapter>,
    _enclosing: Option<Instant>,
) -> Result<(), EngineError> {
    if let Some(plan) = plan {
        super::gated_launch::revalidate_post_spawn(plan)
            .map_err(|error| EngineError::Container(error.to_string()))?;
    }
    Ok(())
}

fn install_lifecycle_cancel(
    config: &mut BridgeConfig,
    slot: Arc<ChildLifecycleSlot>,
    cli: ContainerCli,
    container_name: String,
    orchestrated: bool,
) {
    config.cancel_on_grace_expired = Some(Arc::new(move || {
        let Some(authority) = slot.authority() else {
            return;
        };
        if !orchestrated {
            stop_and_remove(cli.bin, &container_name);
        }
        let _ = authority.terminate_local_cli(Instant::now() + Duration::from_secs(2));
    }));
}

// ─── The three spawn paths ──────────────────────────────────────────────────

/// Spawn `<bin> run -it` via `portable-pty` and bridge the PTY master to the
/// frontend's `AgentIo` channels via the shared I/O bridge.
///
/// `post_bridge` is the backend's attach hook; `None` means the backend has a
/// native attach and needs no rendezvous socket (Docker).
// The frozen post-start error shape carries complete child and resource custody.
#[allow(clippy::result_large_err)]
pub(super) fn spawn_pty_bridged(
    cli: ContainerCli,
    mut instance: Box<ContainerInstance>,
    req: SpawnRequest,
    post_bridge: Option<AttachHook>,
) -> Result<AgentExecution, SpawnStageError> {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};

    let SpawnRequest {
        io,
        argv,
        seeded: _,
        started_at,
        handle,
        mut bridge_cfg,
        prepared,
        slot,
        gated,
        retention,
        #[cfg(test)]
        fault,
    } = req;

    let (prepared, slot) = ensure_child_lifecycle(prepared, slot).map_err(before_cli_start)?;
    retention.initialize_plan(gated.as_ref().map(|context| context.plan.clone()));

    // Move the credential leases out of the instance (dropped before this fn
    // returns) and into the execution backend, so each lease brackets the child
    // process it credentials.
    let leases = std::mem::take(&mut instance.leases);
    let gate_cleanup = instance.gate_cleanup.take();

    let (cols, rows) = io.initial_size.expect("PTY path requires initial_size");
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| before_cli_start(EngineError::Container(format!("openpty: {e}"))))?;
    assert_leases_before_spawn(&instance.options, &leases);
    let (attach_store, attach_permit) = PreallocatedAttachStore::pair();
    let custody = Box::new(LifecycleResources::Spawn(SpawnCustodyResources {
        io: Some(SpawnIoResources::Pty(pair)),
        leases: Some(leases),
        gate_cleanup,
        post_wait: cli.post_wait,
        run_post_wait: true,
        attach_store,
        attach_permit: Some(attach_permit),
        #[cfg(test)]
        custody_witness: test_support::take_custody_witness(),
    }));

    let mut cmd = CommandBuilder::new(cli.bin);
    for arg in &argv {
        cmd.arg(arg);
    }
    // Agent credentials and the resolved `env()` passthrough values are passed
    // as name-only `-e KEY` in argv; set their values on the child's
    // environment so the CLI resolves them without the secret ever touching the
    // argument vector. The passthrough values cannot rely on ambient
    // inheritance — inside the squad daemon they live in the Layer 0 overlay,
    // not in the daemon's own process environment.
    for (k, v) in &instance.options.agent_credentials {
        cmd.env(k, v);
    }
    for (k, v) in super::docker::resolve_env_passthrough(&instance.options) {
        cmd.env(k, v);
    }

    let (plan, adapter, enclosing) = prepare_spawn_gate(gated).map_err(before_cli_start)?;

    let spawn_custody = custody.spawn().ok_or_else(|| {
        before_cli_start(EngineError::Container(
            "preallocated PTY custody unavailable".into(),
        ))
    })?;
    let child = match spawn_custody.io.as_ref() {
        None => {
            return Err(before_cli_start(EngineError::Container(
                "preallocated PTY custody unavailable".into(),
            )))
        }
        Some(SpawnIoResources::Pty(pair)) => pair.slave.spawn_command(cmd).map_err(|e| {
            before_cli_start(EngineError::Container(format!(
                "spawn {} via pty: {e}",
                cli.bin
            )))
        })?,
        Some(SpawnIoResources::Piped(_)) => {
            return Err(before_cli_start(EngineError::Container(
                "preallocated PTY custody has wrong I/O kind".into(),
            )))
        }
    };
    let lifecycle = prepared
        .bind_started_child_with_resources(SpawnedCreateCli::Pty(child), started_at, custody)
        .map_err(|error| {
            after_cli_start(
                &retention,
                EngineError::Container("create CLI state unknown".into()),
                error.into_retained(),
                LaunchRetentionReason::ChildStateUnknown,
            )
        })?;
    slot.bind(lifecycle.clone()).map_err(|authority| {
        after_cli_start(
            &retention,
            EngineError::Container("create CLI state unknown".into()),
            RetainedExecution::Managed {
                execution: None,
                lifecycle: authority,
            },
            LaunchRetentionReason::ChildStateUnknown,
        )
    })?;
    install_lifecycle_cancel(
        &mut bridge_cfg,
        Arc::clone(&slot),
        cli,
        instance.name.0.clone(),
        plan.is_some(),
    );
    validate_after_spawn(&plan, adapter.as_deref(), enclosing).map_err(|source| {
        after_cli_start(
            &retention,
            source,
            RetainedExecution::Managed {
                execution: None,
                lifecycle: lifecycle.clone(),
            },
            LaunchRetentionReason::PostSpawnControlChanged,
        )
    })?;
    #[cfg(test)]
    if fault == Some(SpawnFault::AfterBindBeforeBridge) {
        return Err(after_cli_start(
            &retention,
            EngineError::Container("create CLI bridge setup failed".into()),
            RetainedExecution::Managed {
                execution: None,
                lifecycle,
            },
            LaunchRetentionReason::BridgeSetupFailed,
        ));
    }

    // Interactive PTY runs pass the seeded prompt as a CLI positional arg
    // (appended by `build_run_argv`), so it must NOT also be written to stdin.
    // Writing it here would cause the PTY to echo the prompt text into the
    // terminal output, painting it over the TUI before the agent starts.

    // Only a backend with an attach hook needs a live tap on the output
    // stream; without one the bridge behaves exactly as it always has.
    let output_broadcast = post_bridge.map(|_| {
        let (tx, _) = tokio::sync::broadcast::channel::<Vec<u8>>(256);
        Arc::new(tx)
    });
    if let Some(tx) = &output_broadcast {
        bridge_cfg.output_broadcast = Some(Arc::clone(tx));
    }

    let wants_attach = post_bridge.is_some();
    let (bridge, resource_loan) = lifecycle
        .transform_resources::<LifecycleResources, _>(move |resources| {
            let LifecycleResources::Spawn(spawn) = resources else {
                return Err(EngineError::Container(
                    "create CLI PTY resource transformation failed".into(),
                ));
            };
            let Some(parts) = spawn.take_execution_parts() else {
                return Err(EngineError::Container(
                    "create CLI PTY resource transformation failed".into(),
                ));
            };
            let SpawnExecutionParts {
                io: io_resources,
                leases,
                gate_cleanup,
                post_wait,
                attach_store,
                attach_permit,
                #[cfg(test)]
                custody_witness,
            } = parts;
            let pair = match io_resources {
                SpawnIoResources::Pty(pair) => pair,
                wrong_kind => {
                    spawn.restore_execution_parts(SpawnExecutionParts {
                        io: wrong_kind,
                        leases,
                        gate_cleanup,
                        post_wait,
                        attach_store,
                        attach_permit,
                        #[cfg(test)]
                        custody_witness,
                    });
                    return Err(EngineError::Container(
                        "create CLI PTY resource kind changed".into(),
                    ));
                }
            };
            let (master_arc, bridge) =
                match crate::engine::container::io_bridge::bridge_pty_owned(io, pair, bridge_cfg) {
                    Ok(bridge) => bridge,
                    Err(failure) => {
                        let crate::engine::container::io_bridge::PtyBridgeFailure {
                            error,
                            pair,
                            reader,
                            io,
                            mut config,
                        } = failure;
                        config.cancel_on_grace_expired = None;
                        *resources = LifecycleResources::FailedPty(FailedPtyBridgeResources {
                            _pair: pair,
                            _reader: reader,
                            _io: io,
                            _config: config,
                            _custody: SpawnCustodyResources {
                                io: None,
                                leases: Some(leases),
                                gate_cleanup,
                                post_wait,
                                run_post_wait: true,
                                attach_store,
                                attach_permit: Some(attach_permit),
                                #[cfg(test)]
                                custody_witness,
                            },
                        });
                        return Err(error);
                    }
                };
            let crate::engine::container::io_bridge::BridgeResult {
                stdin_injector,
                stuck_tx,
                output_tail,
            } = bridge;
            let attach = match (wants_attach, output_broadcast) {
                (true, Some(output_broadcast)) => Some((
                    attach_permit,
                    AttachHookCtxOwned {
                        output_broadcast,
                        stdin_injector: stdin_injector.clone(),
                        pty_master: Arc::downgrade(&master_arc),
                    },
                )),
                _ => {
                    drop(attach_permit);
                    None
                }
            };
            *resources = LifecycleResources::Execution(ExecutionResources {
                post_wait,
                pty_master: Some(master_arc),
                stdin_injector: Some(stdin_injector),
                _attach_store: attach_store,
                leases,
                gate_cleanup,
                #[cfg(test)]
                _custody_witness: custody_witness,
            });
            Ok(ExecutionBridgeArtifacts {
                stuck_tx,
                output_tail,
                attach,
            })
        })
        .map_err(|source| {
            after_cli_start(
                &retention,
                source,
                RetainedExecution::Managed {
                    execution: None,
                    lifecycle: lifecycle.clone(),
                },
                LaunchRetentionReason::BridgeSetupFailed,
            )
        })?;

    if let (Some(hook), Some((permit, attach))) = (post_bridge, bridge.attach) {
        if let Some(guard) = hook(AttachHookCtx {
            container_name: &instance.name.0,
            output_broadcast: attach.output_broadcast,
            stdin_injector: attach.stdin_injector,
            pty_master: attach.pty_master,
        }) {
            permit.store(guard);
        }
    }

    let backend = ContainerExecution {
        cli,
        lifecycle: lifecycle.clone(),
        slot,
        orchestrated: plan.is_some(),
        gated_plan: plan,
        gated_adapter: adapter,
        gated_enclosing: enclosing,
        launch_retention: instance.launch_retention.as_ref().map(Arc::downgrade),
        container_name: instance.name.0.clone(),
        resource_loan: Some(resource_loan),
    };
    Ok(AgentExecution::new(
        handle,
        Box::new(backend),
        bridge.stuck_tx,
        Some(bridge.output_tail),
    ))
}

/// Spawn `<bin> run` with piped stdio and bridge through `AgentIo`.
// The frozen post-start error shape carries complete child and resource custody.
#[allow(clippy::result_large_err)]
pub(super) fn spawn_piped(
    cli: ContainerCli,
    mut instance: Box<ContainerInstance>,
    req: SpawnRequest,
) -> Result<AgentExecution, SpawnStageError> {
    let SpawnRequest {
        io,
        argv,
        seeded,
        started_at,
        handle,
        mut bridge_cfg,
        prepared,
        slot,
        gated,
        retention,
        #[cfg(test)]
        fault,
    } = req;

    let (prepared, slot) = ensure_child_lifecycle(prepared, slot).map_err(before_cli_start)?;
    retention.initialize_plan(gated.as_ref().map(|context| context.plan.clone()));

    let mut cmd = piped_command(cli, &argv, &instance.options);

    // Move leases into the backend; assert one exists before spawn (INV-6).
    let leases = std::mem::take(&mut instance.leases);
    let gate_cleanup = instance.gate_cleanup.take();
    assert_leases_before_spawn(&instance.options, &leases);

    let (attach_store, attach_permit) = PreallocatedAttachStore::pair();
    let mut custody = Box::new(LifecycleResources::Spawn(SpawnCustodyResources {
        io: Some(SpawnIoResources::Piped(
            crate::engine::container::io_bridge::PipedChildIo::empty(),
        )),
        leases: Some(leases),
        gate_cleanup,
        post_wait: cli.post_wait,
        run_post_wait: true,
        attach_store,
        attach_permit: Some(attach_permit),
        #[cfg(test)]
        custody_witness: test_support::take_custody_witness(),
    }));

    let (plan, adapter, enclosing) = prepare_spawn_gate(gated).map_err(before_cli_start)?;
    let pipes = match custody
        .spawn_mut()
        .and_then(|resources| resources.io.as_mut())
    {
        Some(SpawnIoResources::Piped(pipes)) => pipes,
        Some(SpawnIoResources::Pty(_)) | None => {
            return Err(before_cli_start(EngineError::Container(
                "preallocated pipe custody unavailable".into(),
            )))
        }
    };
    let mut child = spawn_child(cli, &mut cmd).map_err(before_cli_start)?;
    pipes.take_from(&mut child);
    let lifecycle = prepared
        .bind_started_child_with_resources(SpawnedCreateCli::Piped(child), started_at, custody)
        .map_err(|error| {
            after_cli_start(
                &retention,
                EngineError::Container("create CLI state unknown".into()),
                error.into_retained(),
                LaunchRetentionReason::ChildStateUnknown,
            )
        })?;
    slot.bind(lifecycle.clone()).map_err(|authority| {
        after_cli_start(
            &retention,
            EngineError::Container("create CLI state unknown".into()),
            RetainedExecution::Managed {
                execution: None,
                lifecycle: authority,
            },
            LaunchRetentionReason::ChildStateUnknown,
        )
    })?;
    install_lifecycle_cancel(
        &mut bridge_cfg,
        Arc::clone(&slot),
        cli,
        instance.name.0.clone(),
        plan.is_some(),
    );
    validate_after_spawn(&plan, adapter.as_deref(), enclosing).map_err(|source| {
        after_cli_start(
            &retention,
            source,
            RetainedExecution::Managed {
                execution: None,
                lifecycle: lifecycle.clone(),
            },
            LaunchRetentionReason::PostSpawnControlChanged,
        )
    })?;
    #[cfg(test)]
    if fault == Some(SpawnFault::AfterBindBeforeBridge) {
        return Err(after_cli_start(
            &retention,
            EngineError::Container("create CLI bridge setup failed".into()),
            RetainedExecution::Managed {
                execution: None,
                lifecycle,
            },
            LaunchRetentionReason::BridgeSetupFailed,
        ));
    }

    // Write seeded prompt into stdin channel before the writer task starts.
    if let Some(prompt) = seeded {
        let _ = io.stdin_tx.send(prompt.into_bytes());
        let _ = io.stdin_tx.send(b"\n".to_vec());
    }

    let (bridge, resource_loan) = bridge_piped_resources(&lifecycle, io, bridge_cfg, false)
        .map_err(|source| {
            after_cli_start(
                &retention,
                source,
                RetainedExecution::Managed {
                    execution: None,
                    lifecycle: lifecycle.clone(),
                },
                LaunchRetentionReason::ChildStateUnknown,
            )
        })?;

    // Non-interactive (piped) path does not retain the stdin injector, so the
    // writer task sees EOF after draining the seeded prompt and closes the
    // child's stdin pipe. Without this, an agent that probes stdin for EOF
    // would hang waiting for input that will never come.
    // `try_inject_stdin` falls back to launching a fresh container — which
    // is the correct behaviour for a non-interactive run that has already
    // consumed its single prompt.
    let backend = ContainerExecution {
        cli,
        lifecycle: lifecycle.clone(),
        slot,
        orchestrated: plan.is_some(),
        gated_plan: plan,
        gated_adapter: adapter,
        gated_enclosing: enclosing,
        launch_retention: instance.launch_retention.as_ref().map(Arc::downgrade),
        container_name: instance.name.0.clone(),
        resource_loan: Some(resource_loan),
    };
    Ok(AgentExecution::new(
        handle,
        Box::new(backend),
        bridge.stuck_tx,
        Some(bridge.output_tail),
    ))
}

/// Spawn `<bin> run -i` (no PTY) with piped stdio for an ACP session and bridge
/// through `AgentIo`.
///
/// The persistent-piped sibling of [`spawn_piped`]. The one difference — and
/// the whole point — is that it does **not** `drop(bridge.stdin_injector)`: the
/// stdin channel is retained on the [`ContainerExecution`] for the session's
/// entire lifetime, exactly as [`spawn_pty_bridged`] keeps its PTY master
/// alive. That keeps the container's stdin pipe open so the ACP driver can
/// write JSON-RPC request lines (via `try_inject_stdin`) across a full
/// bidirectional exchange, instead of the one-write-then-EOF an ordinary
/// non-interactive run performs.
///
/// No seeded prompt is written to stdin here: an ACP session delivers its
/// prompt over the JSON-RPC channel (`session/prompt`), so writing raw text to
/// stdin would corrupt the newline-delimited JSON-RPC framing.
/// `SpawnRequest::seeded` is ignored, and accepted only so the three spawn
/// paths share one request type.
///
/// Security: identical wiring to [`spawn_piped`] — the bytes ride the stdio
/// pipes `-i` already wires up. No ports, no `--network`, no new mounts (see
/// `aspec/architecture/security.md`).
// The frozen post-start error shape carries complete child and resource custody.
#[allow(clippy::result_large_err)]
pub(super) fn spawn_piped_interactive(
    cli: ContainerCli,
    mut instance: Box<ContainerInstance>,
    req: SpawnRequest,
) -> Result<AgentExecution, SpawnStageError> {
    let SpawnRequest {
        io,
        argv,
        seeded: _,
        started_at,
        handle,
        mut bridge_cfg,
        prepared,
        slot,
        gated,
        retention,
        #[cfg(test)]
        fault,
    } = req;

    let (prepared, slot) = ensure_child_lifecycle(prepared, slot).map_err(before_cli_start)?;
    retention.initialize_plan(gated.as_ref().map(|context| context.plan.clone()));

    let mut cmd = piped_command(cli, &argv, &instance.options);

    // Move leases into the backend; assert one exists before spawn (INV-6).
    let leases = std::mem::take(&mut instance.leases);
    let gate_cleanup = instance.gate_cleanup.take();
    assert_leases_before_spawn(&instance.options, &leases);

    let (attach_store, attach_permit) = PreallocatedAttachStore::pair();
    let mut custody = Box::new(LifecycleResources::Spawn(SpawnCustodyResources {
        io: Some(SpawnIoResources::Piped(
            crate::engine::container::io_bridge::PipedChildIo::empty(),
        )),
        leases: Some(leases),
        gate_cleanup,
        post_wait: cli.post_wait,
        run_post_wait: true,
        attach_store,
        attach_permit: Some(attach_permit),
        #[cfg(test)]
        custody_witness: test_support::take_custody_witness(),
    }));

    let (plan, adapter, enclosing) = prepare_spawn_gate(gated).map_err(before_cli_start)?;
    let pipes = match custody
        .spawn_mut()
        .and_then(|resources| resources.io.as_mut())
    {
        Some(SpawnIoResources::Piped(pipes)) => pipes,
        Some(SpawnIoResources::Pty(_)) | None => {
            return Err(before_cli_start(EngineError::Container(
                "preallocated persistent pipe custody unavailable".into(),
            )))
        }
    };
    let mut child = spawn_child(cli, &mut cmd).map_err(before_cli_start)?;
    pipes.take_from(&mut child);
    let lifecycle = prepared
        .bind_started_child_with_resources(
            SpawnedCreateCli::PersistentPiped(child),
            started_at,
            custody,
        )
        .map_err(|error| {
            after_cli_start(
                &retention,
                EngineError::Container("create CLI state unknown".into()),
                error.into_retained(),
                LaunchRetentionReason::ChildStateUnknown,
            )
        })?;
    slot.bind(lifecycle.clone()).map_err(|authority| {
        after_cli_start(
            &retention,
            EngineError::Container("create CLI state unknown".into()),
            RetainedExecution::Managed {
                execution: None,
                lifecycle: authority,
            },
            LaunchRetentionReason::ChildStateUnknown,
        )
    })?;
    install_lifecycle_cancel(
        &mut bridge_cfg,
        Arc::clone(&slot),
        cli,
        instance.name.0.clone(),
        plan.is_some(),
    );
    validate_after_spawn(&plan, adapter.as_deref(), enclosing).map_err(|source| {
        after_cli_start(
            &retention,
            source,
            RetainedExecution::Managed {
                execution: None,
                lifecycle: lifecycle.clone(),
            },
            LaunchRetentionReason::PostSpawnControlChanged,
        )
    })?;
    #[cfg(test)]
    if fault == Some(SpawnFault::AfterBindBeforeBridge) {
        return Err(after_cli_start(
            &retention,
            EngineError::Container("create CLI bridge setup failed".into()),
            RetainedExecution::Managed {
                execution: None,
                lifecycle,
            },
            LaunchRetentionReason::BridgeSetupFailed,
        ));
    }

    let (bridge, resource_loan) = bridge_piped_resources(&lifecycle, io, bridge_cfg, true)
        .map_err(|source| {
            after_cli_start(
                &retention,
                source,
                RetainedExecution::Managed {
                    execution: None,
                    lifecycle: lifecycle.clone(),
                },
                LaunchRetentionReason::ChildStateUnknown,
            )
        })?;

    // Persistent-piped (ACP) path: KEEP the stdin_injector alive (do NOT drop
    // it, unlike `spawn_piped`). Retaining the sender both enables
    // `try_inject_stdin` and prevents the writer task from ever seeing EOF, so
    // the container's stdin pipe stays open for the whole JSON-RPC session.
    let backend = ContainerExecution {
        cli,
        lifecycle: lifecycle.clone(),
        slot,
        orchestrated: plan.is_some(),
        gated_plan: plan,
        gated_adapter: adapter,
        gated_enclosing: enclosing,
        launch_retention: instance.launch_retention.as_ref().map(Arc::downgrade),
        container_name: instance.name.0.clone(),
        resource_loan: Some(resource_loan),
    };
    Ok(AgentExecution::new(
        handle,
        Box::new(backend),
        bridge.stuck_tx,
        Some(bridge.output_tail),
    ))
}

/// `<bin> stop <name>` then `<bin> rm <name>`, both best-effort. `stop` SIGTERMs
/// and then SIGKILLs after the runtime's own grace period; a nonzero exit (the
/// container is already gone) is fine.
///
/// The one place the stop-then-remove pair is written: `ContainerExecution`'s
/// cancel paths and `ContainerBackend::stop` all route through it.
pub(super) fn stop_and_remove(bin: &str, name: &str) {
    let _ = Command::new(bin)
        .args(["stop", name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = Command::new(bin)
        .args(["rm", name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

// ─── The execution backend ──────────────────────────────────────────────────

/// The running container. One type for every CLI-shaped backend; the only
/// per-backend state is `cli`.
pub(super) struct ContainerExecution {
    /// Which CLI owns this container — the binary `cancel` shells out to and
    /// the name that appears in this backend's error messages.
    cli: ContainerCli,
    lifecycle: ChildLifecycleAuthority,
    slot: Arc<ChildLifecycleSlot>,
    orchestrated: bool,
    #[allow(dead_code)]
    gated_plan: Option<DurableLaunchPlan>,
    #[allow(dead_code)]
    gated_adapter: Option<Arc<dyn GatedProviderAdapter>>,
    #[allow(dead_code)]
    gated_enclosing: Option<Instant>,
    container_name: String,
    #[allow(dead_code)]
    launch_retention: Option<Weak<LaunchRetentionRegistry>>,
    resource_loan: Option<ExecutionResourceLoan>,
}

impl Drop for ContainerExecution {
    fn drop(&mut self) {
        drop(self.resource_loan.take());
    }
}

impl ExecutionBackend for ContainerExecution {
    fn wait_blocking(mut self: Box<Self>) -> Result<AgentExitInfo, EngineError> {
        let info = self.lifecycle.wait_actual()?;
        if let Some(resource_loan) = self.resource_loan.take() {
            resource_loan.release_and_wait();
        }
        Ok(info)
    }

    fn try_inject_stdin(&self, bytes: &[u8]) -> Result<bool, EngineError> {
        let tx = self
            .lifecycle
            .with_execution_resources::<LifecycleResources, _>(|resources| {
                resources
                    .execution()
                    .and_then(|resources| resources.stdin_injector.clone())
            })?;
        if let Some(tx) = tx {
            tx.send(bytes.to_vec())
                .map_err(|e| EngineError::Container(format!("inject stdin: {e}")))?;
            return Ok(true);
        }
        Ok(false)
    }

    fn cancel(&self) -> Result<(), EngineError> {
        let authority = self
            .slot
            .authority()
            .ok_or_else(|| EngineError::Container("create CLI state unknown".into()))?;
        if !self.orchestrated {
            stop_and_remove(self.cli.bin, &self.container_name);
        }
        authority
            .terminate_local_cli(Instant::now() + Duration::from_secs(2))
            .map(|_| ())
    }

    fn cancel_handle(&self) -> Option<crate::engine::agent_runtime::execution::CancelHandle> {
        let bin = self.cli.bin;
        let name = self.container_name.clone();
        let slot = Arc::clone(&self.slot);
        let orchestrated = self.orchestrated;
        Some(crate::engine::agent_runtime::execution::CancelHandle::new(
            move || {
                let authority = slot
                    .authority()
                    .ok_or_else(|| EngineError::Container("create CLI state unknown".into()))?;
                if !orchestrated {
                    stop_and_remove(bin, &name);
                }
                authority
                    .terminate_local_cli(Instant::now() + Duration::from_secs(2))
                    .map(|_| ())
            },
        ))
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::engine::container::gated_launch::child_lifecycle_test_support::{
        prepare_with_fault, prepare_with_pause, ActorPausePoint, PollPause, PrepareFault,
    };
    use crate::engine::container::gated_launch::PreparedChildLifecycle;
    use std::cell::RefCell;

    thread_local! {
        static PREPARED_OVERRIDE: RefCell<Option<(PreparedChildLifecycle, Arc<ChildLifecycleSlot>)>> =
            const { RefCell::new(None) };
        static CUSTODY_WITNESS: RefCell<Option<CustodyWitness>> = const { RefCell::new(None) };
    }

    pub(crate) struct CustodyWitness {
        releases: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Drop for CustodyWitness {
        fn drop(&mut self) {
            self.releases
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        }
    }

    #[derive(Clone)]
    pub(crate) struct CustodyWitnessProbe {
        releases: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl CustodyWitnessProbe {
        pub(crate) fn releases(&self) -> usize {
            self.releases.load(std::sync::atomic::Ordering::Acquire)
        }
    }

    fn custody_witness_pair() -> (CustodyWitness, CustodyWitnessProbe) {
        let releases = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        (
            CustodyWitness {
                releases: Arc::clone(&releases),
            },
            CustodyWitnessProbe { releases },
        )
    }

    pub(crate) fn take_custody_witness() -> Option<CustodyWitness> {
        CUSTODY_WITNESS.with(|witness| witness.borrow_mut().take())
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub(crate) enum FixtureIoMode {
        Pty { cols: u16, rows: u16 },
        Piped,
        PersistentPiped,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub(crate) enum FixtureSpawnFault {
        ActorThreadStart,
        BindDisconnected,
        BindFull,
        AfterBindBeforeBridge,
    }

    pub(crate) struct FixtureSpawnGate {
        pub plan: DurableLaunchPlan,
        pub adapter: Arc<dyn GatedProviderAdapter>,
        pub enclosing: Instant,
    }

    pub(crate) struct FixtureSpawnSpec {
        pub mode: FixtureIoMode,
        pub executable: &'static str,
        pub args: Vec<String>,
        pub seeded_prompt: Option<String>,
        pub grace_timeout: Duration,
        pub stuck_timeout: Duration,
        pub gate: Option<FixtureSpawnGate>,
        pub fault: Option<FixtureSpawnFault>,
    }

    pub(crate) struct FixtureSpawnResult {
        pub result: Result<AgentExecution, SpawnStageError>,
        pub slot: Arc<ChildLifecycleSlot>,
        pub stdout: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
        // Frozen fixture output keeps both streams available to later assertions.
        #[allow(dead_code)]
        pub stderr: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
        retention: Arc<SpawnRetentionContext>,
    }

    pub(crate) struct PausedFixtureSpawn {
        pause: Option<PollPause>,
        pub slot: Arc<ChildLifecycleSlot>,
        result: std::sync::mpsc::Receiver<FixtureSpawnResult>,
        worker: Option<std::thread::JoinHandle<()>>,
        custody: CustodyWitnessProbe,
    }

    impl PausedFixtureSpawn {
        pub(crate) fn wait_until_paused(&self, deadline: Instant) -> bool {
            self.pause
                .as_ref()
                .is_some_and(|pause| pause.wait_until_paused(deadline))
        }

        pub(crate) fn resume(&mut self) {
            drop(self.pause.take());
        }

        pub(crate) fn result_until(&self, deadline: Instant) -> Option<FixtureSpawnResult> {
            let remaining = deadline.checked_duration_since(Instant::now())?;
            self.result.recv_timeout(remaining).ok()
        }

        pub(crate) fn join_finished(&mut self) -> bool {
            let Some(worker) = self.worker.take() else {
                return true;
            };
            if worker.is_finished() {
                let _ = worker.join();
                true
            } else {
                self.worker = Some(worker);
                false
            }
        }

        pub(crate) fn custody_probe(&self) -> CustodyWitnessProbe {
            self.custody.clone()
        }
    }

    impl Drop for PausedFixtureSpawn {
        fn drop(&mut self) {
            self.resume();
            if let Some(authority) = self.slot.authority() {
                let _ = authority.terminate_local_cli(Instant::now() + Duration::from_secs(2));
            }
            if let Ok(result) = self.result.recv_timeout(Duration::from_secs(3)) {
                drop(result);
            }
            if let Some(worker) = self.worker.take() {
                if worker.is_finished() {
                    let _ = worker.join();
                }
            }
        }
    }

    pub(crate) fn spawn_fixture_paused(
        spec: FixtureSpawnSpec,
        registry: Arc<LaunchRetentionRegistry>,
        point: ActorPausePoint,
    ) -> Result<PausedFixtureSpawn, EngineError> {
        let (prepared, pause) = prepare_with_pause(point)?;
        let (custody_witness, custody) = custody_witness_pair();
        let slot = Arc::new(ChildLifecycleSlot::new());
        let worker_slot = Arc::clone(&slot);
        let runtime = tokio::runtime::Handle::try_current().ok();
        let (result_tx, result) = std::sync::mpsc::sync_channel(1);
        let worker = std::thread::Builder::new()
            .name("awman-paused-fixture".into())
            .spawn(move || {
                let _runtime_guard = runtime.as_ref().map(tokio::runtime::Handle::enter);
                PREPARED_OVERRIDE.with(|prepared_override| {
                    *prepared_override.borrow_mut() = Some((prepared, worker_slot));
                });
                CUSTODY_WITNESS.with(|witness| {
                    *witness.borrow_mut() = Some(custody_witness);
                });
                let fixture = spawn_fixture(spec, registry);
                let _ = result_tx.send(fixture);
            })
            .map_err(|_| EngineError::Container("paused fixture thread unavailable".into()))?;
        Ok(PausedFixtureSpawn {
            pause: Some(pause),
            slot,
            result,
            worker: Some(worker),
            custody,
        })
    }

    pub(crate) fn spawn_fixture_with_custody_witness(
        spec: FixtureSpawnSpec,
        registry: Arc<LaunchRetentionRegistry>,
    ) -> (FixtureSpawnResult, CustodyWitnessProbe) {
        let (witness, probe) = custody_witness_pair();
        CUSTODY_WITNESS.with(|slot| *slot.borrow_mut() = Some(witness));
        (spawn_fixture(spec, registry), probe)
    }

    impl FixtureSpawnResult {
        pub(crate) fn finish(
            self,
            registry: Arc<LaunchRetentionRegistry>,
        ) -> Result<AgentExecution, EngineError> {
            finish_spawn(Some(registry), self.retention, self.result)
        }
    }

    pub(crate) fn spawn_fixture(
        spec: FixtureSpawnSpec,
        registry: Arc<LaunchRetentionRegistry>,
    ) -> FixtureSpawnResult {
        let prepared_override = PREPARED_OVERRIDE.with(|prepared| prepared.borrow_mut().take());
        let slot = prepared_override
            .as_ref()
            .map(|(_, slot)| Arc::clone(slot))
            .unwrap_or_else(|| Arc::new(ChildLifecycleSlot::new()));
        let retention = SpawnRetentionContext::new(None);
        let prepare_fault = match spec.fault {
            Some(FixtureSpawnFault::ActorThreadStart) => Some(PrepareFault::ThreadStart),
            Some(FixtureSpawnFault::BindDisconnected) => Some(PrepareFault::BindDisconnected),
            Some(FixtureSpawnFault::BindFull) => Some(PrepareFault::BindFull),
            _ => None,
        };
        let prepared = match prepared_override {
            Some((prepared, _)) => Ok(prepared),
            None => match prepare_fault {
                Some(fault) => prepare_with_fault(fault),
                None => ChildLifecycleAuthority::prepare(),
            },
        };
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                let (_stdout_tx, stdout) = tokio::sync::mpsc::unbounded_channel();
                let (_stderr_tx, stderr) = tokio::sync::mpsc::unbounded_channel();
                return FixtureSpawnResult {
                    result: Err(before_cli_start(error)),
                    slot,
                    stdout,
                    stderr,
                    retention,
                };
            }
        };

        let (stdout_tx, stdout) = tokio::sync::mpsc::unbounded_channel();
        let (stderr_tx, stderr) = tokio::sync::mpsc::unbounded_channel();
        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
        let (initial_size, resize) = match spec.mode {
            FixtureIoMode::Pty { cols, rows } => {
                let (_resize_tx, resize_rx) = tokio::sync::mpsc::unbounded_channel();
                (Some((cols, rows)), Some(resize_rx))
            }
            _ => (None, None),
        };
        let io = AgentIo {
            stdout: stdout_tx,
            stderr: stderr_tx,
            stdin_tx,
            stdin_rx,
            resize,
            initial_size,
        };
        let name = spec
            .gate
            .as_ref()
            .map(|gate| gate.plan.key.container_name.clone())
            .unwrap_or_else(|| ContainerName::new("awman-p2-fixture"));
        let image = ImageRef::new("awman-p2-fixture:local");
        let options = ResolvedContainerOptions {
            name: Some(name.clone()),
            image: Some(image.clone()),
            seeded_prompt: spec.seeded_prompt.clone(),
            acp: spec.mode == FixtureIoMode::PersistentPiped,
            ..ResolvedContainerOptions::default()
        };
        let cli = ContainerCli {
            bin: spec.executable,
            label: "",
            start_delay: Duration::ZERO,
            post_wait: no_post_wait,
        };
        let instance = Box::new(ContainerInstance::new_with_launch_retention(
            cli,
            image.clone(),
            name.clone(),
            options,
            Vec::new(),
            None,
            Some(registry),
        ));
        let handle = handle_now(&ContainerId::new(name.0.clone()), &name, &image);
        let gated = spec.gate.map(|gate| GatedSpawnContext {
            plan: gate.plan,
            adapter: gate.adapter,
            enclosing: gate.enclosing,
        });
        let request = SpawnRequest {
            io,
            argv: spec.args,
            seeded: spec.seeded_prompt,
            started_at: chrono::Utc::now(),
            handle,
            bridge_cfg: bridge_config_for(cli, &name, spec.grace_timeout, spec.stuck_timeout),
            prepared: Some(prepared),
            slot: Some(Arc::clone(&slot)),
            gated,
            retention: Arc::clone(&retention),
            fault: (spec.fault == Some(FixtureSpawnFault::AfterBindBeforeBridge))
                .then_some(SpawnFault::AfterBindBeforeBridge),
        };
        let result = match spec.mode {
            FixtureIoMode::Pty { .. } => spawn_pty_bridged(cli, instance, request, None),
            FixtureIoMode::Piped => spawn_piped(cli, instance, request),
            FixtureIoMode::PersistentPiped => spawn_piped_interactive(cli, instance, request),
        };
        FixtureSpawnResult {
            result,
            slot,
            stdout,
            stderr,
            retention,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::container::options::{ContainerOption, ResolvedContainerOptions};

    fn resolve(opts: Vec<ContainerOption>) -> ResolvedContainerOptions {
        ResolvedContainerOptions::resolve(opts).expect("resolve")
    }

    #[test]
    fn container_cli_constants_carry_only_the_per_backend_difference() {
        assert_eq!(ContainerCli::DOCKER.bin, "docker");
        assert_eq!(ContainerCli::APPLE.bin, "container");
        assert_eq!(ContainerCli::DOCKER.label, ContainerCli::APPLE.label);
        assert_eq!(ContainerCli::DOCKER.start_delay, Duration::ZERO);
        assert_eq!(
            ContainerCli::APPLE.start_delay,
            crate::engine::container::timing::APPLE_CONTAINER_START_DELAY
        );
    }

    #[test]
    fn bridge_config_takes_its_start_delay_from_the_cli() {
        let name = ContainerName::new("awman-cfg-test");
        let docker = bridge_config_for(
            ContainerCli::DOCKER,
            &name,
            Duration::from_secs(1),
            Duration::from_secs(2),
        );
        assert_eq!(docker.container_start_delay, Duration::ZERO);
        assert_eq!(docker.grace_timeout, Duration::from_secs(1));
        assert_eq!(docker.stuck_timeout, Duration::from_secs(2));
        assert!(docker.cancel_on_grace_expired.is_some());
        assert!(docker.output_broadcast.is_none());

        let apple = bridge_config_for(
            ContainerCli::APPLE,
            &name,
            Duration::from_secs(1),
            Duration::from_secs(2),
        );
        assert_eq!(
            apple.container_start_delay,
            crate::engine::container::timing::APPLE_CONTAINER_START_DELAY
        );
    }

    /// A stand-in CLI whose `bin` is a script on disk. `bin` is `&'static str`,
    /// so the temp path is leaked — the process is about to end anyway, and it
    /// keeps `ContainerCli` a plain `Copy` constant in production.
    fn fake_cli(path: &std::path::Path) -> ContainerCli {
        ContainerCli {
            bin: Box::leak(path.to_string_lossy().into_owned().into_boxed_str()),
            label: "awman=true",
            start_delay: Duration::ZERO,
            post_wait: no_post_wait,
        }
    }

    #[cfg(unix)]
    fn write_fake_cli(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("fake-cli");
        std::fs::write(&path, body).unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).unwrap();
        path
    }

    #[cfg(unix)]
    fn piped_test_request(
        argv: Vec<String>,
        seeded: Option<String>,
    ) -> (SpawnRequest, AgentHandle) {
        use tokio::sync::mpsc;

        let (stdout_tx, _stdout_rx) = mpsc::unbounded_channel();
        let (stderr_tx, _stderr_rx) = mpsc::unbounded_channel();
        let (stdin_tx, stdin_rx) = mpsc::unbounded_channel();
        let io = AgentIo {
            stdout: stdout_tx,
            stderr: stderr_tx,
            stdin_tx,
            stdin_rx,
            resize: None,
            initial_size: None,
        };
        let handle = AgentHandle {
            id: "spawn-test".into(),
            image_tag: "img:latest".into(),
            name: "spawn-test".into(),
            started_at: chrono::Utc::now(),
        };
        let bridge_cfg = BridgeConfig {
            grace_timeout: Duration::from_secs(60),
            stuck_timeout: Duration::from_secs(60),
            container_start_delay: Duration::ZERO,
            cancel_on_grace_expired: None,
            output_tail: Arc::new(
                crate::engine::agent_runtime::output_tail::OutputTail::with_default_capacity(),
            ),
            output_broadcast: None,
        };
        (
            SpawnRequest {
                io,
                argv,
                seeded,
                started_at: chrono::Utc::now(),
                handle: handle.clone(),
                bridge_cfg,
                prepared: None,
                slot: None,
                gated: None,
                fault: None,
                retention: SpawnRetentionContext::new(None),
            },
            handle,
        )
    }

    #[cfg(unix)]
    fn test_instance(cli: ContainerCli, opts: Vec<ContainerOption>) -> Box<ContainerInstance> {
        let image = ImageRef::new("img:latest");
        let name = ContainerName::new("spawn-test");
        Box::new(ContainerInstance::new(
            cli,
            image,
            name,
            resolve(opts),
            Vec::new(),
            None,
        ))
    }

    /// The ACP path must retain its stdin injector for the whole session; the
    /// one-shot piped path must not. This is the behaviour the two backends
    /// each used to assert (or, in Apple's case, not assert) separately.
    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_piped_interactive_keeps_stdin_injector_after_seeded_write() {
        let tmp = tempfile::tempdir().unwrap();
        // A stand-in for `<bin> run`: keep stdin open long enough for this test
        // to distinguish the persistent ACP path from the one-shot
        // `spawn_piped` path, which drops its injector after seeding.
        let cli = fake_cli(&write_fake_cli(tmp.path(), "#!/bin/sh\nsleep 1\n"));

        let image = ImageRef::new("img:latest");
        let instance = test_instance(
            cli,
            vec![
                ContainerOption::Image(image),
                ContainerOption::Interactive(true),
                ContainerOption::Acp(true),
            ],
        );
        let (req, _handle) = piped_test_request(
            vec!["run".into(), "-i".into(), "img:latest".into()],
            // The optional seed is accepted for signature parity; a later
            // injection must still work in ACP mode.
            Some("seeded".into()),
        );

        let mut execution = spawn_piped_interactive(cli, instance, req).unwrap();
        assert!(
            execution.try_inject_stdin(b"seeded\n").unwrap(),
            "ACP spawn must retain stdin_injector after the optional seed"
        );
        let exit = execution.wait().await.unwrap();
        assert_eq!(exit.exit_code, 0);
    }

    /// The one-shot piped path drops its injector so the child sees EOF.
    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_piped_drops_the_stdin_injector_after_seeding() {
        let tmp = tempfile::tempdir().unwrap();
        let cli = fake_cli(&write_fake_cli(tmp.path(), "#!/bin/sh\ncat >/dev/null\n"));

        let image = ImageRef::new("img:latest");
        let instance = test_instance(cli, vec![ContainerOption::Image(image)]);
        let (req, _handle) = piped_test_request(
            vec!["run".into(), "img:latest".into()],
            Some("hello".into()),
        );

        let mut execution = spawn_piped(cli, instance, req).unwrap();
        assert!(
            !execution.try_inject_stdin(b"more\n").unwrap(),
            "one-shot piped spawn must have dropped its stdin_injector"
        );
        // The seeded prompt reached the child, which then saw EOF and exited.
        let exit = execution.wait().await.unwrap();
        assert_eq!(exit.exit_code, 0);
    }

    /// A missing binary is reported as a runtime-unavailable error naming that
    /// backend's binary — the message both backends used to build by hand.
    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_piped_reports_a_missing_binary_as_runtime_unavailable() {
        let tmp = tempfile::tempdir().unwrap();
        let cli = fake_cli(&tmp.path().join("definitely-not-installed"));

        let image = ImageRef::new("img:latest");
        let instance = test_instance(cli, vec![ContainerOption::Image(image)]);
        let (req, _handle) = piped_test_request(vec!["run".into()], None);

        let err = match spawn_piped(cli, instance, req) {
            Ok(_) => panic!("spawn must fail when the binary does not exist"),
            Err(err) => err,
        };
        let err = match err {
            SpawnStageError::BeforeCliStart(source) => source,
            SpawnStageError::AfterCliStart { .. } => {
                panic!("missing binary unexpectedly returned child custody")
            }
        };
        match err {
            EngineError::ContainerRuntimeUnavailable { binary } => {
                assert_eq!(binary, cli.bin)
            }
            other => panic!("expected ContainerRuntimeUnavailable, got {other:?}"),
        }
    }

    /// WI 0116 §1/D1, the second half of the fix: `build_run_argv` emits the
    /// name only, so something has to put the value where the container CLI can
    /// find it. Inside the squad daemon that value lives in the Layer 0 overlay
    /// and *not* in the daemon's own environment, so ambient inheritance —
    /// which is what carried a passthrough before WI 0116 — cannot reach it.
    #[test]
    fn piped_command_carries_the_passthrough_value_on_the_child_never_in_argv() {
        use crate::data::config::env::{
            set_daemon_overlay, DaemonEnvMap, DAEMON_OVERLAY_TEST_LOCK,
        };
        use crate::engine::container::options::EnvVar;

        let _guard = DAEMON_OVERLAY_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let name = "AWMAN_TEST_PROCESS_OVERLAY_ONLY";
        std::env::remove_var(name);
        set_daemon_overlay(DaemonEnvMap::from_pairs([(name, "overlay-secret")]));

        let options = resolve(vec![
            ContainerOption::Image(crate::engine::container::options::ImageRef::new(
                "img:latest",
            )),
            ContainerOption::EnvPassthrough(EnvVar(name.into())),
        ]);
        let argv = vec!["run".to_string(), "-e".to_string(), name.to_string()];
        let cmd = piped_command(ContainerCli::DOCKER, &argv, &options);

        let injected: Vec<(String, String)> = cmd
            .get_envs()
            .filter_map(|(k, v)| {
                Some((
                    k.to_string_lossy().into_owned(),
                    v?.to_string_lossy().into_owned(),
                ))
            })
            .collect();
        assert!(
            injected.contains(&(name.to_string(), "overlay-secret".to_string())),
            "the value must be set on the child's environment; got {injected:?}"
        );
        assert!(
            !cmd.get_args()
                .any(|a| a.to_string_lossy().contains("overlay-secret")),
            "and it must never appear in argv"
        );

        set_daemon_overlay(DaemonEnvMap::new());
    }
}
