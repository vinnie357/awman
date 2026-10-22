//! A reaped child does not make an unfinished resource destructor disappear.

use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::gated_launch::child_lifecycle_test_support::lifecycle_probe;
use super::gated_launch::{ChildLifecycleAuthority, SpawnedCreateCli};

struct HeldResource {
    entered: mpsc::SyncSender<()>,
    release: mpsc::Receiver<()>,
}

impl Drop for HeldResource {
    fn drop(&mut self) {
        let _ = self.entered.send(());
        let _ = self.release.recv_timeout(Duration::from_secs(5));
    }
}

struct ReleaseOnDrop(Option<mpsc::SyncSender<()>>);

impl ReleaseOnDrop {
    fn release(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.release();
    }
}

#[test]
fn reaped_child_resource_wait_honors_deadline_while_destructor_is_held() {
    let prepared = ChildLifecycleAuthority::prepare().expect("prepare actual lifecycle");
    let child = Command::new("/bin/sh")
        .args(["-c", "exit 23"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("owned local child, never a provider");
    let pid = child.id();
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let mut release = ReleaseOnDrop(Some(release_tx));
    let authority = prepared
        .bind_started_child_with_resources(
            SpawnedCreateCli::Piped(child),
            chrono::Utc::now(),
            Box::new(HeldResource {
                entered: entered_tx,
                release: release_rx,
            }),
        )
        .expect("bind child and physical resource custody");
    let (_, mut loan) = authority
        .transform_resources::<HeldResource, _>(|_| Ok(()))
        .expect("obtain the production execution resource loan");
    let actual = authority
        .wait_actual_until(Instant::now() + Duration::from_secs(1))
        .expect("actor actual wait")
        .expect("owned child must exit");
    assert_eq!(actual.exit_code, 23);
    let probe = lifecycle_probe(&authority);
    let before = probe.snapshot();
    assert_eq!(before.bound_pid, Some(pid));
    assert_eq!(before.native_reaps, 1);
    assert!(!before.owns_unreaped_child);
    assert!(before.resources_present);

    let (outcome_tx, outcome_rx) = mpsc::sync_channel(1);
    let (retry_tx, retry_rx) = mpsc::sync_channel(1);
    let (finished_tx, finished_rx) = mpsc::sync_channel(1);
    let waiter = std::thread::spawn(move || {
        let bounded = loan.release_and_wait_until(Instant::now() + Duration::from_millis(100));
        let _ = outcome_tx.send(bounded);
        if retry_rx.recv_timeout(Duration::from_secs(2)).is_ok() {
            let released = loan.release_and_wait_until(Instant::now() + Duration::from_secs(1));
            let _ = finished_tx.send(released);
        }
    });
    let destructor_entered = entered_rx.recv_timeout(Duration::from_secs(1));
    let while_held = outcome_rx.recv_timeout(Duration::from_millis(500));
    let held = probe.snapshot();

    release.release();
    let _ = retry_tx.send(());
    let after_release = finished_rx.recv_timeout(Duration::from_secs(2));
    if after_release.is_ok() {
        waiter.join().expect("completed waiter must not panic");
    }

    assert!(destructor_entered.is_ok(), "controlled destructor must run");
    assert_eq!(held.actual_exit, Some(actual));
    assert_eq!(held.native_reaps, 1);
    assert!(!held.owns_unreaped_child);
    assert!(
        held.resources_present,
        "physical custody must remain pending"
    );
    assert!(
        !while_held.expect("bounded resource wait exceeded its deadline while destructor was held"),
        "an unfinished destructor must never report released resources"
    );
    assert!(after_release.expect("released resource wait must finish"));
    assert!(authority.resources_are_released());
}
