use super::*;

#[derive(Clone)]
pub(crate) struct RetentionProbe {
    shared: Arc<Shared>,
}

pub(crate) struct SupervisorActiveClaimBarrier {
    shared: Arc<Shared>,
}

impl Drop for SupervisorActiveClaimBarrier {
    fn drop(&mut self) {
        self.shared
            .supervisor_active_claims_blocked
            .store(false, Ordering::Release);
        self.shared.changed.notify_all();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ActiveGatedRetentionSnapshot {
    pub ticket: LaunchRetentionTicket,
    pub ticket_present: bool,
    pub entry_present: bool,
    pub resource_loans: usize,
    pub resource_loan_releases: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ActiveGatedResolutionSnapshot {
    pub ticket: LaunchRetentionTicket,
    pub ticket_present: bool,
    pub entry_present: bool,
    pub resource_loans: usize,
    pub resource_loan_releases: usize,
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

    pub(crate) fn wake_supervisor(&self) {
        self.shared.changed.notify_all();
    }

    pub(crate) fn hold_supervisor_active_claims(&self) -> SupervisorActiveClaimBarrier {
        let already_blocked = self
            .shared
            .supervisor_active_claims_blocked
            .swap(true, Ordering::AcqRel);
        assert!(
            !already_blocked,
            "supervisor active-claim barrier is already held"
        );
        SupervisorActiveClaimBarrier {
            shared: Arc::clone(&self.shared),
        }
    }

    pub(crate) fn advance_one_supervisor_active_attempt(
        &self,
    ) -> Result<Option<LaunchRetentionTicket>, EngineError> {
        if !self
            .shared
            .supervisor_active_claims_blocked
            .load(Ordering::Acquire)
        {
            return Err(EngineError::Container(
                "supervisor active-claim barrier must be held".into(),
            ));
        }
        let shutdown = self.shared.shutdown_requested.load(Ordering::Acquire);
        supervisor_iteration(&self.shared, shutdown, false)
    }

    fn only_active_gated_snapshot(
        &self,
        entries: &BTreeMap<LaunchRetentionTicket, RetainedEntry>,
    ) -> Option<ActiveGatedRetentionSnapshot> {
        let mut active = entries.iter().filter_map(|(ticket, entry)| {
            let RetainedRegistryEnvelope::ActiveGated(custody) = &entry.launch else {
                return None;
            };
            Some((*ticket, custody))
        });
        let (ticket, custody) = active.next()?;
        if active.next().is_some() {
            return None;
        }
        let tickets = self
            .shared
            .tickets
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let releases = self
            .shared
            .resource_loan_releases
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        Some(ActiveGatedRetentionSnapshot {
            ticket,
            ticket_present: tickets.contains(&ticket),
            entry_present: true,
            resource_loans: usize::from(custody.resource_loan.is_some()),
            resource_loan_releases: releases.get(&ticket).copied().unwrap_or(0),
        })
    }

    pub(crate) fn wait_until_only_active_gated_retained(
        &self,
        deadline: Instant,
    ) -> Option<ActiveGatedRetentionSnapshot> {
        let mut entries = self
            .shared
            .entries
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        loop {
            if let Some(snapshot) = self.only_active_gated_snapshot(&entries) {
                if snapshot.ticket_present && snapshot.resource_loans == 1 {
                    return Some(snapshot);
                }
            }
            let remaining = deadline.checked_duration_since(Instant::now())?;
            let (next, timeout) = self
                .shared
                .changed
                .wait_timeout(entries, remaining)
                .unwrap_or_else(|p| p.into_inner());
            entries = next;
            if timeout.timed_out() {
                return self
                    .only_active_gated_snapshot(&entries)
                    .filter(|snapshot| snapshot.ticket_present && snapshot.resource_loans == 1);
            }
        }
    }

    fn resolved_snapshot(
        &self,
        entries: &BTreeMap<LaunchRetentionTicket, RetainedEntry>,
        observed: ActiveGatedRetentionSnapshot,
    ) -> Option<ActiveGatedResolutionSnapshot> {
        if !observed.ticket_present || !observed.entry_present || observed.resource_loans != 1 {
            return None;
        }
        let tickets = self
            .shared
            .tickets
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let ticket_present = tickets.contains(&observed.ticket);
        let entry = entries.get(&observed.ticket);
        let entry_present = entry.is_some();
        let resource_loans = match entry.map(|entry| &entry.launch) {
            Some(RetainedRegistryEnvelope::ActiveGated(custody)) => {
                usize::from(custody.resource_loan.is_some())
            }
            Some(RetainedRegistryEnvelope::Gated(_) | RetainedRegistryEnvelope::Legacy(_)) => {
                return None;
            }
            None => 0,
        };
        let releases = self
            .shared
            .resource_loan_releases
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&observed.ticket)
            .copied()
            .unwrap_or(0);
        (!ticket_present && !entry_present).then_some(ActiveGatedResolutionSnapshot {
            ticket: observed.ticket,
            ticket_present,
            entry_present,
            resource_loans,
            resource_loan_releases: releases,
        })
    }

    pub(crate) fn wait_until_observed_ticket_resolved(
        &self,
        observed: ActiveGatedRetentionSnapshot,
        deadline: Instant,
    ) -> Option<ActiveGatedResolutionSnapshot> {
        let mut entries = self
            .shared
            .entries
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        loop {
            if let Some(snapshot) = self.resolved_snapshot(&entries, observed) {
                return Some(snapshot);
            }
            let remaining = deadline.checked_duration_since(Instant::now())?;
            let (next, timeout) = self
                .shared
                .changed
                .wait_timeout(entries, remaining)
                .unwrap_or_else(|p| p.into_inner());
            entries = next;
            if timeout.timed_out() {
                return self.resolved_snapshot(&entries, observed);
            }
        }
    }

    pub(crate) fn active_gated_resource_loans(&self, ticket: LaunchRetentionTicket) -> usize {
        let entries = self
            .shared
            .entries
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        match entries.get(&ticket).map(|entry| &entry.launch) {
            Some(RetainedRegistryEnvelope::ActiveGated(custody)) => {
                usize::from(custody.resource_loan.is_some())
            }
            Some(RetainedRegistryEnvelope::Gated(_) | RetainedRegistryEnvelope::Legacy(_))
            | None => 0,
        }
    }

    pub(crate) fn resource_loan_releases(&self, ticket: LaunchRetentionTicket) -> usize {
        self.shared
            .resource_loan_releases
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&ticket)
            .copied()
            .unwrap_or(0)
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
