//! Lease registry — the RAII proof that a credentialed container is live.
//!
//! A [`CredentialLease`] is handed out by [`LeaseRegistry::register`] at the
//! single container-spawn choke point and is owned by the container's execution
//! backend, so it deregisters in [`Drop`] on **every** exit path — normal exit,
//! spawn failure, error propagation, panic unwind and Ctrl-C teardown alike
//! (INV-6). The monitor reads a point-in-time [`LeaseSnapshot`] list each tick
//! and re-checks [`LeaseRegistry::is_live`] before every write, so a lease
//! dropped mid-tick loses the race safely (INV-7).
//!
//! [`LeaseGeneration`] is monotonic and never reused: a recycled staged path
//! necessarily carries a *different* generation, which is the second of the
//! three independent defenses against writing into a dead session's directory.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::Duration;

use crate::data::session::AgentName;
use crate::engine::auth::RefreshableCredentialDelivery;

/// Monotonic id, unique per registration for the process's lifetime. Reused
/// paths get a NEW generation, so a stale tick can never write into a recycled
/// path (INV-7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LeaseGeneration(u64);

impl LeaseGeneration {
    /// The underlying counter value. For logging/tests only.
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

/// Registry-side record of one live lease. Carries no secret.
#[derive(Debug, Clone)]
struct LeaseRecord {
    agent: AgentName,
    spec_agent: &'static str,
    staged_path: PathBuf,
    staged_root: PathBuf,
    container: String,
}

#[derive(Default)]
struct RegistryInner {
    entries: HashMap<LeaseGeneration, LeaseRecord>,
    next_gen: u64,
    shutdown: bool,
}

/// Shared state behind the registry. The `Arc` is held by the [`LeaseRegistry`]
/// and (for parking/shutdown) by the monitor's background thread; each
/// [`CredentialLease`] holds only a `Weak` so a live lease never keeps the
/// registry alive on its own.
pub(super) struct RegistryShared {
    inner: Mutex<RegistryInner>,
    /// Signalled on empty→non-empty transitions and on shutdown so the monitor
    /// loop can park at zero CPU while no credentialed container is live.
    wake: Condvar,
}

impl RegistryShared {
    fn deregister(&self, generation: LeaseGeneration) {
        let mut inner = self.inner.lock().unwrap();
        inner.entries.remove(&generation);
        // No wake needed on empty: the monitor loop parks itself the next time
        // it observes an empty registry.
    }

    /// Block until at least one lease is registered or shutdown is requested.
    /// Returns `true` while there is work to do, `false` once shutdown.
    pub(super) fn wait_until_active(&self) -> bool {
        let mut inner = self.inner.lock().unwrap();
        while inner.entries.is_empty() && !inner.shutdown {
            inner = self.wake.wait(inner).unwrap();
        }
        !inner.shutdown
    }

    /// Sleep up to `dur`, returning early if shutdown is requested. Returns
    /// `true` when shutdown was requested.
    pub(super) fn sleep_or_shutdown(&self, dur: Duration) -> bool {
        let inner = self.inner.lock().unwrap();
        let (inner, _timed_out) = self
            .wake
            .wait_timeout_while(inner, dur, |st| !st.shutdown)
            .unwrap();
        inner.shutdown
    }

    pub(super) fn request_shutdown(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.shutdown = true;
        self.wake.notify_all();
    }
}

/// RAII guard proving a credentialed container is live. Deregisters in [`Drop`].
///
/// It is MOVED into the container's execution backend by every spawn function,
/// because the instance box is dropped before the spawn function returns; the
/// backend is consumed when the child exits, so the lease's lifetime brackets
/// the child process exactly. Deliberately NOT `Clone`.
pub struct CredentialLease {
    registry: Weak<RegistryShared>,
    generation: LeaseGeneration,
    agent: AgentName,
    staged_path: PathBuf,
    container: String,
}

impl CredentialLease {
    pub fn generation(&self) -> LeaseGeneration {
        self.generation
    }

    pub fn agent(&self) -> &AgentName {
        &self.agent
    }

    /// Absolute staged credential-file path this lease covers.
    pub fn staged_path(&self) -> &Path {
        &self.staged_path
    }

    /// Container name, for logging only.
    pub fn container(&self) -> &str {
        &self.container
    }
}

impl Drop for CredentialLease {
    fn drop(&mut self) {
        if let Some(shared) = self.registry.upgrade() {
            shared.deregister(self.generation);
        }
    }
}

impl std::fmt::Debug for CredentialLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialLease")
            .field("generation", &self.generation)
            .field("agent", &self.agent)
            .field("container", &self.container)
            .field("staged_path", &self.staged_path)
            .finish()
    }
}

/// A tick-time copy of one live lease. Carries no secret.
#[derive(Debug, Clone)]
pub struct LeaseSnapshot {
    pub generation: LeaseGeneration,
    pub agent: AgentName,
    pub spec_agent: &'static str,
    pub staged_path: PathBuf,
    pub staged_root: PathBuf,
    pub container: String,
}

/// The process-wide set of live credential leases.
pub struct LeaseRegistry {
    shared: Arc<RegistryShared>,
}

impl Default for LeaseRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for LeaseRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LeaseRegistry")
            .field("live", &self.len())
            .finish()
    }
}

impl LeaseRegistry {
    pub fn new() -> Self {
        Self {
            shared: Arc::new(RegistryShared {
                inner: Mutex::new(RegistryInner::default()),
                wake: Condvar::new(),
            }),
        }
    }

    /// Register a live credentialed container and hand back the RAII guard.
    /// Called ONLY from the container backends' `build()` via the monitor.
    pub fn register(
        &self,
        delivery: &RefreshableCredentialDelivery,
        container: &str,
    ) -> CredentialLease {
        let mut inner = self.shared.inner.lock().unwrap();
        let was_empty = inner.entries.is_empty();
        let generation = LeaseGeneration(inner.next_gen);
        inner.next_gen += 1;
        inner.entries.insert(
            generation,
            LeaseRecord {
                agent: delivery.agent.clone(),
                spec_agent: delivery.spec_agent,
                staged_path: delivery.staged_path.clone(),
                staged_root: delivery.staged_root.clone(),
                container: container.to_string(),
            },
        );
        if was_empty {
            // Wake the parked monitor loop on the empty→non-empty transition.
            self.shared.wake.notify_all();
        }
        drop(inner);
        CredentialLease {
            registry: Arc::downgrade(&self.shared),
            generation,
            agent: delivery.agent.clone(),
            staged_path: delivery.staged_path.clone(),
            container: container.to_string(),
        }
    }

    /// Point-in-time copy of every live lease.
    pub fn snapshot(&self) -> Vec<LeaseSnapshot> {
        let inner = self.shared.inner.lock().unwrap();
        inner
            .entries
            .iter()
            .map(|(generation, rec)| LeaseSnapshot {
                generation: *generation,
                agent: rec.agent.clone(),
                spec_agent: rec.spec_agent,
                staged_path: rec.staged_path.clone(),
                staged_root: rec.staged_root.clone(),
                container: rec.container.clone(),
            })
            .collect()
    }

    /// Is this generation still registered? Re-checked immediately before every
    /// write; a mismatch is a SKIP (INV-7).
    pub fn is_live(&self, generation: LeaseGeneration) -> bool {
        self.shared
            .inner
            .lock()
            .unwrap()
            .entries
            .contains_key(&generation)
    }

    pub fn is_empty(&self) -> bool {
        self.shared.inner.lock().unwrap().entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.shared.inner.lock().unwrap().entries.len()
    }

    /// Clone the shared handle for the monitor's background thread (parking and
    /// shutdown only).
    pub(super) fn shared(&self) -> Arc<RegistryShared> {
        Arc::clone(&self.shared)
    }

    /// Request the monitor loop to stop (used by the monitor's `Drop`).
    pub(super) fn request_shutdown(&self) {
        self.shared.request_shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::auth::credential::CredentialFingerprint;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn delivery(agent: &str) -> RefreshableCredentialDelivery {
        let dir = std::env::temp_dir().join(format!("awman-lease-test-{agent}"));
        RefreshableCredentialDelivery {
            agent: AgentName::new(agent).unwrap(),
            spec_agent: "claude",
            credential_env_key: "CLAUDE_CODE_OAUTH_TOKEN",
            staged_path: dir.join(".credentials.json"),
            staged_root: dir,
            initial_fingerprint: CredentialFingerprint::zeroed(),
        }
    }

    #[cfg(unix)]
    const READY_NONCE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[cfg(unix)]
    fn write_protected(path: &Path, bytes: &[u8]) {
        std::fs::write(path, bytes).expect("write protected gate record");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .expect("protect gate record");
    }

    #[cfg(unix)]
    fn write_json(path: &Path, value: &serde_json::Value) {
        write_protected(
            path,
            &serde_json::to_vec(value).expect("serialize gate record"),
        );
    }

    #[cfg(unix)]
    fn valid_json_padded_to(value: &serde_json::Value, exact_len: usize) -> Vec<u8> {
        let mut raw = serde_json::to_vec(value).expect("serialize padded gate record");
        assert!(raw.len() <= exact_len, "fixture must fit requested length");
        raw.resize(exact_len, b' ');
        raw
    }

    #[cfg(unix)]
    fn request_record() -> serde_json::Value {
        serde_json::json!({
            "version": 1,
            "bindings": [{
                "id": "review-input",
                "workspace_path": "/review/input",
                "manifest_id": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "manifest_file": "review.manifest.json",
                "access": "read-only"
            }]
        })
    }

    #[cfg(unix)]
    fn ready_record(container: &str) -> serde_json::Value {
        serde_json::json!({
            "version": 1,
            "nonce": READY_NONCE,
            "container_name": container,
            "bindings": [{
                "id": "review-input",
                "workspace_path": "/review/input",
                "manifest_id": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "access": "read-only"
            }]
        })
    }

    #[cfg(unix)]
    struct PendingGateFixture {
        control: tempfile::TempDir,
        registry: LeaseRegistry,
        _lease: CredentialLease,
    }

    #[cfg(unix)]
    impl PendingGateFixture {
        fn new() -> Self {
            let control = tempfile::tempdir().expect("gate control");
            std::fs::set_permissions(control.path(), std::fs::Permissions::from_mode(0o700))
                .expect("protect gate control");
            write_json(&control.path().join("request.json"), &request_record());
            write_json(
                &control.path().join("ready.json"),
                &ready_record("awman-reviewer"),
            );
            let registry = LeaseRegistry::new();
            let lease = registry.register_pending(
                &delivery("claude"),
                "awman-reviewer",
                Some(control.path().to_path_buf()),
            );
            Self {
                control,
                registry,
                _lease: lease,
            }
        }

        fn write_release(&self, nonce: &str) {
            write_json(
                &self.control.path().join(".released"),
                &serde_json::json!({"version": 1, "nonce": nonce}),
            );
        }

        fn gate_pending(&self) -> bool {
            self.registry
                .snapshot()
                .into_iter()
                .next()
                .expect("registered lease")
                .gate_pending
        }
    }

    #[cfg(unix)]
    #[test]
    fn full_production_ready_and_two_field_receipt_release_once_and_latch() {
        let fixture = PendingGateFixture::new();
        assert!(fixture.gate_pending(), "release receipt is not present yet");
        fixture.write_release(READY_NONCE);
        assert!(
            !fixture.gate_pending(),
            "matching request, container, ready record and receipt must release"
        );

        std::fs::remove_file(fixture.control.path().join("ready.json")).expect("remove ready");
        std::fs::remove_file(fixture.control.path().join(".released")).expect("remove receipt");
        assert!(
            !fixture.gate_pending(),
            "a successful release is monotonic for this lease generation"
        );
    }

    #[cfg(unix)]
    #[test]
    fn wrong_nonce_container_or_binding_identity_stays_pending() {
        let wrong_nonce = PendingGateFixture::new();
        wrong_nonce
            .write_release("cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc");
        assert!(wrong_nonce.gate_pending(), "wrong receipt nonce");

        let wrong_container = PendingGateFixture::new();
        write_json(
            &wrong_container.control.path().join("ready.json"),
            &ready_record("awman-other"),
        );
        wrong_container.write_release(READY_NONCE);
        assert!(wrong_container.gate_pending(), "wrong ready container");

        let wrong_binding = PendingGateFixture::new();
        let mut ready = ready_record("awman-reviewer");
        ready["bindings"][0]["manifest_id"] =
            serde_json::json!("cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc");
        write_json(&wrong_binding.control.path().join("ready.json"), &ready);
        wrong_binding.write_release(READY_NONCE);
        assert!(
            wrong_binding.gate_pending(),
            "ready bindings must identify the current request"
        );
    }

    #[cfg(unix)]
    #[test]
    fn incomplete_malformed_or_failed_records_stay_pending() {
        let incomplete = PendingGateFixture::new();
        write_json(
            &incomplete.control.path().join("ready.json"),
            &serde_json::json!({"version": 1, "nonce": READY_NONCE}),
        );
        incomplete.write_release(READY_NONCE);
        assert!(incomplete.gate_pending(), "legacy two-field ready record");

        let malformed_ready = PendingGateFixture::new();
        write_protected(&malformed_ready.control.path().join("ready.json"), b"{");
        malformed_ready.write_release(READY_NONCE);
        assert!(malformed_ready.gate_pending(), "invalid ready JSON bytes");

        let malformed_receipt = PendingGateFixture::new();
        write_protected(&malformed_receipt.control.path().join(".released"), b"{");
        assert!(
            malformed_receipt.gate_pending(),
            "invalid receipt JSON bytes"
        );

        let failed = PendingGateFixture::new();
        failed.write_release(READY_NONCE);
        write_json(
            &failed.control.path().join("failure.json"),
            &serde_json::json!({
                "version": 1,
                "code": "release-timeout",
                "message": "release was not consumed"
            }),
        );
        assert!(
            failed.gate_pending(),
            "a failure record must prevent a fabricated release from winning"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unsafe_or_oversized_marker_files_stay_pending() {
        let unsafe_ready_mode = PendingGateFixture::new();
        unsafe_ready_mode.write_release(READY_NONCE);
        std::fs::set_permissions(
            unsafe_ready_mode.control.path().join("ready.json"),
            std::fs::Permissions::from_mode(0o644),
        )
        .expect("weaken ready mode");
        assert!(unsafe_ready_mode.gate_pending(), "unsafe ready mode");

        let linked_receipt = PendingGateFixture::new();
        linked_receipt.write_release(READY_NONCE);
        std::fs::hard_link(
            linked_receipt.control.path().join(".released"),
            linked_receipt.control.path().join("receipt-alias"),
        )
        .expect("hard-link receipt");
        assert!(linked_receipt.gate_pending(), "hard-linked receipt");

        let oversized_ready = PendingGateFixture::new();
        let oversized_ready_bytes =
            valid_json_padded_to(&ready_record("awman-reviewer"), 64 * 1024 + 1);
        assert_eq!(oversized_ready_bytes.len(), 64 * 1024 + 1);
        write_protected(
            &oversized_ready.control.path().join("ready.json"),
            &oversized_ready_bytes,
        );
        oversized_ready.write_release(READY_NONCE);
        assert!(
            oversized_ready.gate_pending(),
            "otherwise-valid ready JSON above 64 KiB"
        );

        let oversized_receipt = PendingGateFixture::new();
        let oversized_receipt_bytes = valid_json_padded_to(
            &serde_json::json!({"version": 1, "nonce": READY_NONCE}),
            4097,
        );
        assert_eq!(oversized_receipt_bytes.len(), 4097);
        write_protected(
            &oversized_receipt.control.path().join(".released"),
            &oversized_receipt_bytes,
        );
        assert!(
            oversized_receipt.gate_pending(),
            "otherwise-valid receipt JSON above 4 KiB"
        );
    }

    #[test]
    fn register_then_drop_deregisters() {
        let reg = LeaseRegistry::new();
        assert!(reg.is_empty());
        let lease = reg.register(&delivery("claude"), "awman-x");
        assert_eq!(reg.len(), 1);
        let gen = lease.generation();
        assert!(reg.is_live(gen));
        drop(lease);
        assert!(reg.is_empty());
        assert!(!reg.is_live(gen));
    }

    #[test]
    fn generation_is_monotonic_and_never_reused() {
        let reg = LeaseRegistry::new();
        let a = reg.register(&delivery("claude"), "awman-a").generation();
        // Drop the first lease, then register again: the path is recycled but
        // the generation MUST differ.
        let b = reg.register(&delivery("claude"), "awman-b").generation();
        assert_ne!(a, b);
        assert!(b.as_u64() > a.as_u64());
    }

    #[test]
    fn drop_deregisters_through_panic_unwind() {
        let reg = LeaseRegistry::new();
        let gen = {
            let lease = reg.register(&delivery("claude"), "awman-x");
            let g = lease.generation();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _held = lease;
                panic!("boom");
            }));
            assert!(result.is_err());
            g
        };
        // The lease was owned by the unwinding closure; Drop must have run.
        assert!(!reg.is_live(gen));
        assert!(reg.is_empty());
    }

    #[test]
    fn snapshot_reflects_live_leases_only() {
        let reg = LeaseRegistry::new();
        let l1 = reg.register(&delivery("claude"), "awman-1");
        let _l2 = reg.register(&delivery("claude"), "awman-2");
        assert_eq!(reg.snapshot().len(), 2);
        drop(l1);
        let snap = reg.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].container, "awman-2");
    }

    /// The monitor's second defense (INV-7): a generation captured in a stale
    /// snapshot must read as not-live even after the path it pointed at has
    /// been recycled by a brand-new registration.
    #[test]
    fn is_live_false_for_generation_dropped_and_since_recycled() {
        let reg = LeaseRegistry::new();
        let a = reg.register(&delivery("claude"), "awman-a");
        let stale_generation = a.generation();
        drop(a);
        // Recycle: a new lease takes a fresh generation over the same registry.
        let _b = reg.register(&delivery("claude"), "awman-b");
        assert!(
            !reg.is_live(stale_generation),
            "a dropped lease's generation must never read as live again, \
             even once the registry is non-empty with a different lease"
        );
    }

    #[test]
    fn is_live_false_for_generation_never_registered() {
        let reg = LeaseRegistry::new();
        assert!(!reg.is_live(LeaseGeneration(u64::MAX)));
    }

    /// Proxy for the monitor's tick loop: `RegistryShared::wait_until_active`
    /// parks a waiter while the registry is empty and wakes it exactly on the
    /// empty→non-empty transition caused by the next `register()` call.
    #[test]
    fn registry_parks_on_empty_and_wakes_on_next_registration() {
        let reg = LeaseRegistry::new();
        assert!(reg.is_empty());
        let shared = reg.shared();
        let (tx, rx) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            tx.send(shared.wait_until_active()).unwrap();
        });
        // Give the waiter a chance to actually park; harmless if it hasn't —
        // `register`'s notify only needs to reach an already-parked waiter,
        // and the mutex ordering between wait/notify makes this race-free
        // (see `RegistryShared::wait_until_active`'s doc comment).
        std::thread::sleep(Duration::from_millis(50));
        let lease = reg.register(&delivery("claude"), "awman-restart");
        let active = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("parked waiter must wake once a lease is registered");
        assert!(active, "wait_until_active must report active, not shutdown");
        waiter.join().unwrap();
        drop(lease);
    }

    #[test]
    fn registry_shutdown_wakes_a_parked_waiter_with_inactive() {
        let reg = LeaseRegistry::new();
        let shared = reg.shared();
        let (tx, rx) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            tx.send(shared.wait_until_active()).unwrap();
        });
        std::thread::sleep(Duration::from_millis(50));
        reg.request_shutdown();
        let active = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("parked waiter must wake on shutdown");
        assert!(!active, "shutdown must report inactive, not a live lease");
        waiter.join().unwrap();
    }
}
