//! Owner-runqueue load publication and SMP balancing.

use super::*;

/// One owner-selected migration candidate and destination.
///
/// Selection is intentionally move-only. The owner may revalidate and commit
/// this exact candidate once, but a caller cannot accidentally scan the source
/// runqueue again after choosing a destination.
pub(super) struct OwnerBalanceSelection {
    candidate: QueuedThread,
    target: CpuId,
    now_ns: u64,
    reason: BalanceReason,
}

impl OwnerBalanceSelection {
    pub(super) const fn target(&self) -> CpuId {
        self.target
    }
}

#[cfg(test)]
std::thread_local! {
    static BALANCE_CANDIDATE_VISITS: core::cell::Cell<usize> = const {
        core::cell::Cell::new(0)
    };
    static LOAD_SUMMARY_PUBLICATIONS: core::cell::Cell<usize> = const {
        core::cell::Cell::new(0)
    };
    static OWNER_BALANCE_PASSES: core::cell::Cell<usize> = const {
        core::cell::Cell::new(0)
    };
    static FAIL_BALANCE_TRANSFER_AFTER_DETACH: core::cell::Cell<bool> = const {
        core::cell::Cell::new(false)
    };
    static FAIL_BALANCE_TRANSFER_PUBLICATION_RESERVATION: core::cell::Cell<bool> = const {
        core::cell::Cell::new(false)
    };
}

#[cfg(test)]
pub(super) fn reset_balance_candidate_visits() {
    BALANCE_CANDIDATE_VISITS.set(0);
}

#[cfg(test)]
pub(super) fn balance_candidate_visits() -> usize {
    BALANCE_CANDIDATE_VISITS.get()
}

#[cfg(test)]
pub(super) fn reset_load_summary_publications() {
    LOAD_SUMMARY_PUBLICATIONS.set(0);
}

#[cfg(test)]
pub(super) fn load_summary_publications() -> usize {
    LOAD_SUMMARY_PUBLICATIONS.get()
}

#[cfg(test)]
pub(super) fn reset_owner_balance_passes() {
    OWNER_BALANCE_PASSES.set(0);
}

#[cfg(test)]
pub(super) fn owner_balance_passes() -> usize {
    OWNER_BALANCE_PASSES.get()
}

#[cfg(test)]
fn fail_next_balance_transfer_after_detach() {
    FAIL_BALANCE_TRANSFER_AFTER_DETACH.set(true);
}

#[cfg(test)]
fn fail_next_balance_transfer_publication_reservation() {
    FAIL_BALANCE_TRANSFER_PUBLICATION_RESERVATION.set(true);
}

impl TaskSystem {
    /// Returns the fixed CPU topology width accepted by affinity masks.
    pub const fn cpu_topology_len(&self) -> usize {
        self.config.cpu_count()
    }

    /// Captures stable state for deterministic scheduler comparisons.
    pub fn snapshot(&self, cpu: Pin<&CpuLocal>) -> Result<CpuSnapshot, TaskError> {
        self.ensure_owner_cpu_context(&cpu)?;
        Ok(CpuSnapshot::capture(&cpu))
    }

    /// Returns the number of CPUs currently available for placement.
    pub fn online_cpu_count(&self) -> usize {
        loop {
            let sequence = self.topology_sequence.read_begin();
            let count = self.online_count.load(Ordering::Acquire);
            if !self.topology_sequence.read_retry(sequence) {
                return count;
            }
        }
    }

    pub(super) fn publish_owner_cpu_load_summary(&self, cpu: Pin<&mut CpuLocal>) {
        #[cfg(test)]
        LOAD_SUMMARY_PUBLICATIONS.set(LOAD_SUMMARY_PUBLICATIONS.get().saturating_add(1));
        // Every caller already owns either the scheduler baton or an owner IRQ
        // guard. Like Linux's rq clock/load update under rq ownership, this
        // nested publication needs no second IRQ-state transaction.
        let run_queue = cpu.lock_run_queue();
        cpu.remote().publish_run_queue_load_summary(&run_queue);
    }

    fn select_owner_balance_transfer_by(
        &self,
        cpu: &CpuLocal,
        now_ns: u64,
        reason: BalanceReason,
        mut select_target: impl FnMut(&QueuedThread, &ThreadSchedState) -> Option<CpuId>,
    ) -> Option<OwnerBalanceSelection> {
        let source = cpu.owner();
        let state = cpu.dispatch_state();
        let current_policy = state
            .current_dispatch
            .as_ref()
            .map(CurrentDispatch::schedule_policy);
        let fair_balance_due = cpu.fair_balance_due(now_ns);
        let scan_epoch = cpu.lock_run_queue().begin_balance_scan();
        loop {
            let candidate = {
                let mut run_queue = cpu.lock_run_queue();
                let queued_top_rt = run_queue.highest_rt_priority();
                let top_rt_count =
                    queued_top_rt.map_or(0, |priority| run_queue.rt_count_at_priority(priority));
                run_queue.next_balance_candidate(scan_epoch, |candidate| {
                    #[cfg(test)]
                    BALANCE_CANDIDATE_VISITS.set(BALANCE_CANDIDATE_VISITS.get().saturating_add(1));
                    let class_allowed = match reason {
                        BalanceReason::IdlePull => {
                            !matches!(
                                candidate.policy,
                                SchedulePolicy::Fair {
                                    mode: FairMode::Idle,
                                    ..
                                }
                            ) && (!matches!(candidate.policy, SchedulePolicy::Fair { .. })
                                || fair_balance_due)
                        }
                        BalanceReason::RtDeadlinePush => matches!(
                            candidate.policy,
                            SchedulePolicy::Deadline(_)
                                | SchedulePolicy::Fifo { .. }
                                | SchedulePolicy::RoundRobin { .. }
                        ),
                        BalanceReason::FairPeriodic => matches!(
                            candidate.policy,
                            SchedulePolicy::Fair {
                                mode: FairMode::Normal | FairMode::Batch,
                                ..
                            }
                        ),
                    };
                    if !class_allowed {
                        return false;
                    }
                    let candidate_priority = match candidate.policy {
                        SchedulePolicy::Fifo { priority }
                        | SchedulePolicy::RoundRobin { priority, .. } => priority.get(),
                        _ => return true,
                    };
                    match current_policy {
                        Some(SchedulePolicy::Deadline(_)) => true,
                        Some(SchedulePolicy::Fifo { priority })
                        | Some(SchedulePolicy::RoundRobin { priority, .. }) => {
                            candidate_priority <= priority.get()
                        }
                        _ => queued_top_rt.is_some_and(|top| {
                            candidate_priority < top
                                || (candidate_priority == top && top_rt_count > 1)
                        }),
                    }
                })
            }?;
            let sched = candidate.core.sched().lock();
            let Some(target) = select_target(&candidate, &sched) else {
                continue;
            };
            let target_is_allowed = |target: CpuId| {
                self.cpu_remotes
                    .get(target.as_usize())
                    .is_some_and(|remote| {
                        remote.accepts_placement()
                            && remote.is_scheduler_ready()
                            && sched.placement.affinity.contains(target)
                    })
            };
            let deadline_covers_online =
                !matches!(sched.policy.applied, SchedulePolicy::Deadline(_))
                    || self.cpu_remotes.iter().enumerate().all(|(index, remote)| {
                        !remote.accepts_placement()
                            || sched.placement.affinity.contains(CpuId::new(index as u32))
                    });
            if target == source
                || !target_is_allowed(target)
                || sched.placement.queued_cpu() != Some(source)
                || sched.placement.migration_target().is_some()
                || sched.placement.on_cpu().is_some()
                || candidate.core.sleep_timer_cpu().is_some()
                || !deadline_covers_online
            {
                continue;
            }
            let queued = cpu.lock_run_queue().queued_thread(candidate.id);
            if let Some(queued) = queued {
                return Some(OwnerBalanceSelection {
                    candidate: queued,
                    target,
                    now_ns,
                    reason,
                });
            }
        }
    }

    pub(super) fn select_owner_balance_transfer(
        &self,
        cpu: &CpuLocal,
        target: CpuId,
        now_ns: u64,
        reason: BalanceReason,
    ) -> Option<OwnerBalanceSelection> {
        self.select_owner_balance_transfer_by(cpu, now_ns, reason, |_, _| Some(target))
    }

    pub(super) fn select_rt_deadline_balance_transfer(
        &self,
        cpu: &CpuLocal,
        source_load: usize,
        now_ns: u64,
    ) -> Option<OwnerBalanceSelection> {
        let source = cpu.owner();
        self.select_owner_balance_transfer_by(
            cpu,
            now_ns,
            BalanceReason::RtDeadlinePush,
            |candidate, sched| {
                let key = candidate.balance_key();
                self.cpu_remotes
                    .iter()
                    .enumerate()
                    .filter_map(|(index, remote)| {
                        let target = CpuId::new(index as u32);
                        if target == source
                            || !remote.accepts_placement()
                            || !remote.is_scheduler_ready()
                            || !sched.placement.affinity.contains(target)
                        {
                            return None;
                        }
                        let target_summary = remote.try_load_summary()?;
                        if target_summary.runnable_count() >= source_load
                            || target_summary
                                .current_key()
                                .is_some_and(|current| current <= key && current.class_rank() != 3)
                        {
                            return None;
                        }
                        Some((target_summary.runnable_count(), target))
                    })
                    .min_by_key(|(load, target)| (*load, target.as_u32()))
                    .map(|(_, target)| target)
            },
        )
    }

    pub(super) fn commit_owner_balance_transfer(
        &self,
        mut cpu: Pin<&mut CpuLocal>,
        selection: OwnerBalanceSelection,
    ) -> Result<BalanceTransferOutcome, TaskError> {
        self.ensure_owner_cpu_online(&cpu)?;
        let _irq = IrqScope::enter();
        let OwnerBalanceSelection {
            candidate,
            target,
            now_ns,
            reason,
        } = selection;
        if self
            .cpu_remote(target)
            .is_none_or(|remote| !remote.is_scheduler_ready())
        {
            return Ok(BalanceTransferOutcome::Retry);
        }
        let source = cpu.owner();
        if source == target {
            return Ok(BalanceTransferOutcome::NoCandidate);
        }
        let migrated_fair = matches!(candidate.policy, SchedulePolicy::Fair { .. });
        let core = candidate.core;
        let mut sched = core.sched().lock();
        let deadline_covers_online = !matches!(sched.policy.applied, SchedulePolicy::Deadline(_))
            || self.cpu_remotes.iter().enumerate().all(|(index, remote)| {
                !remote.accepts_placement()
                    || sched.placement.affinity.contains(CpuId::new(index as u32))
            });
        if sched.lifecycle.state() != ThreadState::Ready
            || sched.placement.queued_cpu() != Some(source)
            || sched.placement.migration_target().is_some()
            || sched.placement.on_cpu().is_some()
            || !sched.placement.affinity.contains(target)
            || core.sleep_timer_cpu().is_some()
            || !deadline_covers_online
        {
            return Ok(BalanceTransferOutcome::Retry);
        }
        #[cfg(test)]
        let publication_exit = FAIL_BALANCE_TRANSFER_PUBLICATION_RESERVATION
            .replace(false)
            .then(|| {
                core.close_owned_scheduler_activity()
                    .expect("failure injection requires a quiescent scheduler activity gate")
            });
        let carrier = match self.prepare_owner_migration(&core, source, target) {
            Ok(carrier) => carrier,
            Err(_) => {
                #[cfg(test)]
                drop(publication_exit);
                return Ok(BalanceTransferOutcome::Retry);
            }
        };
        #[cfg(test)]
        drop(publication_exit);
        let detached = {
            let current_fair = cpu
                .dispatch_state()
                .current_dispatch
                .as_ref()
                .and_then(|current| current.entity.fair());
            let Some(detached) = cpu.lock_run_queue().detach_for_transfer(
                core.id(),
                current_fair,
                self.config.timing_granularity_ns(),
            ) else {
                return Ok(BalanceTransferOutcome::Retry);
            };
            detached
        };
        let queued_entity = detached.thread.entity;
        let prepare_result: Result<(), TaskError> = (|| {
            Self::detach_owner_deadline_bandwidth_locked(&core, &mut sched, cpu.as_mut())?;
            #[cfg(test)]
            if FAIL_BALANCE_TRANSFER_AFTER_DETACH.replace(false) {
                return Err(TaskError::InvalidConfiguration);
            }
            sched.policy.effective_entity = queued_entity;
            if !sched.is_pi_boosted() {
                sched.policy.base_entity = queued_entity;
            }
            self.capture_owner_fair_migration(cpu.as_ref().get_ref(), &mut sched);
            sched.placement.begin_queued_migration(source, target)?;
            core.set_wake_cpu_hint(target);
            Ok(())
        })();
        drop(sched);
        if prepare_result.is_err() {
            self.rollback_owner_queued_migration(cpu.as_mut(), &core, detached, source, target)?;
            return Ok(BalanceTransferOutcome::Retry);
        }
        carrier.commit();
        self.publish_owner_cpu_load_summary(cpu.as_mut());
        if migrated_fair && reason != BalanceReason::FairPeriodic {
            let completion_now_ns = Self::scheduler_completion_now_ns(now_ns);
            cpu.as_mut()
                .reset_fair_balance(completion_now_ns, self.config.balance_interval_ns());
        }
        Ok(BalanceTransferOutcome::Migrated(core.id()))
    }

    pub(super) fn transfer_owner_balance_candidate(
        &self,
        cpu: Pin<&mut CpuLocal>,
        target: CpuId,
        now_ns: u64,
        reason: BalanceReason,
    ) -> Result<BalanceTransferOutcome, TaskError> {
        self.ensure_owner_cpu_online(&cpu)?;
        let Some(selection) =
            self.select_owner_balance_transfer(cpu.as_ref().get_ref(), target, now_ns, reason)
        else {
            return Ok(BalanceTransferOutcome::NoCandidate);
        };
        self.commit_owner_balance_transfer(cpu, selection)
    }

    pub(super) fn rollback_owner_queued_migration(
        &self,
        mut cpu: Pin<&mut CpuLocal>,
        core: &Arc<ThreadCore>,
        detached: DetachedQueueEntry,
        source: CpuId,
        target: CpuId,
    ) -> Result<(), TaskError> {
        let state_result = {
            let mut sched = core.sched().lock();
            match sched.placement.rollback_queued_migration(source, target) {
                Err(error) => Err(error),
                Ok(()) => {
                    core.set_wake_cpu_hint(source);
                    sched.policy.effective_entity.cancel_fair_migration();
                    if !sched.is_pi_boosted() {
                        sched.policy.base_entity = sched.policy.effective_entity;
                    } else {
                        sched.policy.base_entity.cancel_fair_migration();
                    }
                    Self::activate_owner_deadline_bandwidth(core, &mut sched, cpu.as_mut(), source)
                        .and_then(|()| {
                            Self::refresh_owner_deadline_timers_locked(
                                core,
                                &mut sched,
                                cpu.as_mut(),
                            )
                        })
                }
            }
        };
        cpu.lock_run_queue().restore_detached(detached);
        self.publish_owner_cpu_load_summary(cpu);
        state_result
    }

    /// Returns whether this owner has scheduler-class balance work to service.
    ///
    /// The owner has just published a coherent runqueue snapshot. Like Linux's
    /// rq balance callbacks, an ordinary context switch is not itself a reason
    /// to enter SMP balancing: idle entry, an overloaded RT/Deadline queue, or
    /// the periodic Fair deadline must request the work explicitly.
    pub(super) fn owner_balance_work_pending(
        &self,
        cpu: &CpuLocal,
        next: ThreadId,
        now_ns: u64,
    ) -> bool {
        if task_runtime::in_hard_irq() {
            return false;
        }
        if cpu.idle() == Some(next) || cpu.fair_balance_due(now_ns) {
            return true;
        }
        cpu.try_load_summary().is_some_and(|summary| {
            summary.is_overloaded()
                && matches!(
                    summary.pushable_class(),
                    Some(SchedulingClass::Deadline | SchedulingClass::Realtime)
                )
        })
    }

    pub(super) fn service_owner_balance(
        &self,
        mut cpu: Pin<&mut CpuLocal>,
        next: ThreadId,
        now_ns: u64,
    ) -> Result<(), TaskError> {
        #[cfg(test)]
        OWNER_BALANCE_PASSES.set(OWNER_BALANCE_PASSES.get().saturating_add(1));
        if cpu.idle() == Some(next) {
            let _requested = self.request_idle_pull(cpu.as_ref())?;
        } else {
            let _pushed = self.push_overloaded_from_published_summary(cpu.as_mut())?;
            let _fair = self.balance_fair(cpu.as_mut(), now_ns)?;
        }
        Ok(())
    }

    pub(super) fn balance_fair(
        &self,
        mut cpu: Pin<&mut CpuLocal>,
        now_ns: u64,
    ) -> Result<Option<ThreadId>, TaskError> {
        if task_runtime::in_hard_irq() || !cpu.fair_balance_due(now_ns) {
            return Ok(None);
        }
        self.ensure_owner_cpu_online(&cpu)?;
        self.publish_owner_cpu_load_summary(cpu.as_mut());
        let source = cpu.owner();
        let result = if let Some(source_load) = cpu.try_runnable_summary() {
            let lower_load_target_seen =
                self.cpu_remotes.iter().enumerate().any(|(index, remote)| {
                    let target = CpuId::new(index as u32);
                    remote.accepts_placement()
                        && target != source
                        && remote
                            .try_load_summary()
                            .is_some_and(|summary| summary.runnable_count() < source_load)
                });
            let selection = self.select_owner_balance_transfer_by(
                cpu.as_ref().get_ref(),
                now_ns,
                BalanceReason::FairPeriodic,
                |_, sched| {
                    self.cpu_remotes
                        .iter()
                        .enumerate()
                        .filter_map(|(index, remote)| {
                            let target = CpuId::new(index as u32);
                            if target == source
                                || !remote.accepts_placement()
                                || !remote.is_scheduler_ready()
                                || !sched.placement.affinity.contains(target)
                            {
                                return None;
                            }
                            let target_summary = remote.try_load_summary()?;
                            (target_summary.runnable_count() < source_load)
                                .then_some((target_summary.runnable_count(), target))
                        })
                        .min_by_key(|(load, target)| (*load, target.as_u32()))
                        .map(|(_, target)| target)
                },
            );
            if let Some(selection) = selection {
                match self.commit_owner_balance_transfer(cpu.as_mut(), selection)? {
                    BalanceTransferOutcome::Migrated(thread) => FairBalanceResult::Migrated(thread),
                    BalanceTransferOutcome::NoCandidate | BalanceTransferOutcome::Retry => {
                        FairBalanceResult::Constrained
                    }
                }
            } else if lower_load_target_seen {
                FairBalanceResult::Constrained
            } else {
                FairBalanceResult::Balanced
            }
        } else {
            FairBalanceResult::Balanced
        };
        let completion_now_ns = Self::scheduler_completion_now_ns(now_ns);
        let minimum_interval_ns = self.config.balance_interval_ns();
        match result {
            FairBalanceResult::Migrated(_) => {
                cpu.as_mut()
                    .reset_fair_balance(completion_now_ns, minimum_interval_ns);
            }
            FairBalanceResult::Balanced => {
                cpu.as_mut().backoff_fair_balance(
                    completion_now_ns,
                    minimum_interval_ns,
                    minimum_interval_ns.saturating_mul(FAIR_BALANCE_BALANCED_BACKOFF_FACTOR),
                );
            }
            FairBalanceResult::Constrained => {
                cpu.as_mut().backoff_fair_balance(
                    completion_now_ns,
                    minimum_interval_ns,
                    minimum_interval_ns.saturating_mul(FAIR_BALANCE_CONSTRAINED_BACKOFF_FACTOR),
                );
            }
        }
        Ok(result.migrated())
    }
}

#[cfg(test)]
mod tests {
    use alloc::boxed::Box;

    use super::*;
    use crate::{DeadlineFlags, DeadlinePolicy, Nice, RtPriority, ThreadSpec};

    fn online_pair() -> (TaskSystem, Pin<Box<CpuLocal>>, Pin<Box<CpuLocal>>) {
        let system = TaskSystem::new(TaskSystemConfig::new(2)).unwrap();
        let mut cpu0 = system.create_cpu_local(CpuId::new(0)).unwrap();
        let mut cpu1 = system.create_cpu_local(CpuId::new(1)).unwrap();
        for cpu in [&mut cpu0, &mut cpu1] {
            system
                .register_idle_thread(
                    cpu.as_mut(),
                    ThreadSpec::new(SchedulePolicy::fair(Nice::ZERO, FairMode::Idle)),
                )
                .unwrap();
            system.bring_cpu_online(cpu.as_mut()).unwrap();
        }
        (system, cpu0, cpu1)
    }

    #[test]
    fn failed_balance_transfer_restores_source_ownership_and_deadline_bandwidth() {
        let (system, mut cpu0, cpu1) = online_pair();
        let policy =
            SchedulePolicy::deadline(DeadlinePolicy::new(2, 10, 20, DeadlineFlags::NONE).unwrap());
        let first = system.create_thread(ThreadSpec::new(policy)).unwrap();
        let second = system.create_thread(ThreadSpec::new(policy)).unwrap();
        for thread in [&first, &second] {
            system.make_ready(thread.id()).unwrap();
            system.enqueue(cpu0.as_mut(), thread.id(), 0).unwrap();
        }
        let bandwidth_before = cpu0.deadline_bandwidth();

        fail_next_balance_transfer_after_detach();
        assert_eq!(
            system.transfer_owner_balance_candidate(
                cpu0.as_mut(),
                CpuId::new(1),
                0,
                BalanceReason::RtDeadlinePush,
            ),
            Ok(BalanceTransferOutcome::Retry)
        );

        assert_eq!(
            cpu0.runnable_count(),
            2,
            "a failed transfer must restore physical source runqueue ownership"
        );
        assert_eq!(cpu1.runnable_count(), 0);
        assert_eq!(
            cpu0.deadline_bandwidth(),
            bandwidth_before,
            "a failed transfer must restore the source Deadline bandwidth ledger"
        );
        let sched = first.core.sched().lock();
        assert_eq!(sched.lifecycle.state(), ThreadState::Ready);
        assert_eq!(sched.placement.queued_cpu(), Some(CpuId::new(0)));
        assert_eq!(sched.placement.migration_target(), None);
        assert_eq!(sched.deadline.bandwidth_cpu, Some(CpuId::new(0)));
    }

    #[test]
    fn failed_balance_transfer_preserves_rt_fifo_position() {
        let (system, mut cpu0, _cpu1) = online_pair();
        let policy = SchedulePolicy::fifo(RtPriority::new(50).unwrap());
        let first = system.create_thread(ThreadSpec::new(policy)).unwrap();
        let second = system.create_thread(ThreadSpec::new(policy)).unwrap();
        for thread in [&first, &second] {
            system.make_ready(thread.id()).unwrap();
            system.enqueue(cpu0.as_mut(), thread.id(), 0).unwrap();
        }

        fail_next_balance_transfer_after_detach();
        assert_eq!(
            system.transfer_owner_balance_candidate(
                cpu0.as_mut(),
                CpuId::new(1),
                0,
                BalanceReason::RtDeadlinePush,
            ),
            Ok(BalanceTransferOutcome::Retry)
        );

        assert_eq!(
            system.schedule(cpu0.as_mut(), 0).unwrap().next(),
            first.id(),
            "rollback must restore the candidate at its original FIFO position"
        );
    }

    #[test]
    fn prepared_balance_transfer_revalidates_target_affinity_before_detach() {
        let (system, mut cpu0, _cpu1) = online_pair();
        let thread = system
            .create_thread(ThreadSpec::new(SchedulePolicy::default()))
            .unwrap();
        system.make_ready(thread.id()).unwrap();
        system.enqueue(cpu0.as_mut(), thread.id(), 0).unwrap();

        let selection = system
            .select_owner_balance_transfer(
                cpu0.as_ref().get_ref(),
                CpuId::new(1),
                0,
                BalanceReason::FairPeriodic,
            )
            .expect("the initial affinity permits a CPU 1 transfer");
        let mut cpu0_only = CpuSet::empty(2);
        assert!(cpu0_only.insert(CpuId::new(0)));
        system.set_affinity(thread.id(), cpu0_only).unwrap();

        assert_eq!(
            system.commit_owner_balance_transfer(cpu0.as_mut(), selection),
            Ok(BalanceTransferOutcome::Retry),
            "a prepared migration must not detach after a concurrent affinity update rejects its \
             target"
        );
        assert_eq!(cpu0.runnable_count(), 1);
    }

    #[test]
    fn committed_local_switch_survives_a_recoverable_balance_race() {
        let (system, mut cpu0, _cpu1) = online_pair();
        let policy = SchedulePolicy::fifo(RtPriority::new(50).unwrap());
        let first = system.create_thread(ThreadSpec::new(policy)).unwrap();
        let second = system.create_thread(ThreadSpec::new(policy)).unwrap();
        for thread in [&first, &second] {
            system.make_ready(thread.id()).unwrap();
            system.enqueue(cpu0.as_mut(), thread.id(), 0).unwrap();
        }

        // Model an affinity/offline/publication race after the owner has
        // detached a balance candidate. The transfer transaction rolls back
        // completely, so the already committed local selection must remain
        // usable instead of being converted into a fatal scheduler error.
        fail_next_balance_transfer_after_detach();
        let decision = system.schedule(cpu0.as_mut(), 0).unwrap();

        assert_eq!(decision.next(), first.id());
        assert_eq!(cpu0.runnable_count(), 1);
    }

    #[test]
    fn failed_migration_reservation_restores_the_source_carrier() {
        let (system, mut cpu0, _cpu1) = online_pair();
        let policy = SchedulePolicy::fifo(RtPriority::new(50).unwrap());
        let first = system.create_thread(ThreadSpec::new(policy)).unwrap();
        let second = system.create_thread(ThreadSpec::new(policy)).unwrap();
        for thread in [&first, &second] {
            system.make_ready(thread.id()).unwrap();
            system.enqueue(cpu0.as_mut(), thread.id(), 0).unwrap();
        }

        fail_next_balance_transfer_publication_reservation();
        assert_eq!(
            system.transfer_owner_balance_candidate(
                cpu0.as_mut(),
                CpuId::new(1),
                0,
                BalanceReason::RtDeadlinePush,
            ),
            Ok(BalanceTransferOutcome::Retry)
        );

        assert_eq!(cpu0.runnable_count(), 2);
        let sched = first.core.sched().lock();
        assert_eq!(sched.placement.queued_cpu(), Some(CpuId::new(0)));
        assert_eq!(sched.placement.migration_target(), None);
        drop(sched);
        assert_eq!(first.assigned_cpu(), Some(CpuId::new(0)));
    }
}
