//! Wake consumption, runqueue dispatch, and policy-application internals.

use super::*;

#[derive(Clone, Copy)]
pub(super) struct PolicyGenerationCommit {
    pub(super) base_policy: SchedulePolicy,
    pub(super) running_policy_changed: bool,
    pub(super) held_deadline_reservation: u64,
    pub(super) committed_deadline_reservation: u64,
}

#[cfg(test)]
std::thread_local! {
    static WAKE_TARGET_SELECTIONS: core::cell::Cell<usize> = const {
        core::cell::Cell::new(0)
    };
    static OWNER_DISPATCH_CONSTRUCTIONS: core::cell::Cell<usize> = const {
        core::cell::Cell::new(0)
    };
}

#[cfg(test)]
pub(super) fn reset_wake_target_selections() {
    WAKE_TARGET_SELECTIONS.set(0);
}

#[cfg(test)]
pub(super) fn wake_target_selections() -> usize {
    WAKE_TARGET_SELECTIONS.get()
}

#[cfg(test)]
pub(super) fn reset_owner_dispatch_constructions() {
    OWNER_DISPATCH_CONSTRUCTIONS.set(0);
}

#[cfg(test)]
pub(super) fn owner_dispatch_constructions() -> usize {
    OWNER_DISPATCH_CONSTRUCTIONS.get()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WakeTransition {
    Notified,
    Activate,
    DeferredUntilSwitchTail,
}

pub(super) struct OwnerDispatchCommit {
    overrun_work: Option<Arc<ThreadCore>>,
}

impl OwnerDispatchCommit {
    const NONE: Self = Self { overrun_work: None };
}

impl TaskSystem {
    fn consume_wake_locked(
        core: &Arc<ThreadCore>,
        sched: &mut ThreadSchedState,
    ) -> Result<WakeTransition, TaskError> {
        let lifecycle = sched.lifecycle.state();
        if !core.consume_wake(lifecycle == ThreadState::Parking) || lifecycle == ThreadState::Exited
        {
            return Ok(WakeTransition::Notified);
        }
        match lifecycle {
            ThreadState::Parking => Ok(WakeTransition::Notified),
            ThreadState::Blocked => {
                sched.transition(core, ThreadState::Waking)?;
                Ok(if sched.placement.on_cpu().is_some() {
                    WakeTransition::DeferredUntilSwitchTail
                } else {
                    WakeTransition::Activate
                })
            }
            ThreadState::Ready | ThreadState::Running | ThreadState::Waking => {
                Ok(WakeTransition::Notified)
            }
            ThreadState::New | ThreadState::Exited => Ok(WakeTransition::Notified),
        }
    }

    fn select_wake_target(
        &self,
        sched: &ThreadSchedState,
        policy: SchedulePolicy,
        entity: SchedulingEntity,
        preferred: Option<CpuId>,
    ) -> Option<CpuId> {
        #[cfg(test)]
        WAKE_TARGET_SELECTIONS.set(WAKE_TARGET_SELECTIONS.get().saturating_add(1));
        self.select_priority_cpu(policy, entity, &sched.affinity.affinity, preferred, None)
    }

    /// Activates a blocked thread directly under its target runqueue lock.
    ///
    /// Lock order is thread scheduler state, then target runqueue. This is the
    /// active PREEMPT_RT wakeup model: no owner inbox or later safe point owns
    /// the transition from blocked to physically queued.
    pub(crate) fn wake_thread_direct(
        &self,
        core: Arc<ThreadCore>,
        preferred: Option<CpuId>,
    ) -> WakeResult {
        #[cfg(feature = "qperf-metrics")]
        crate::metrics::record_direct_wake_attempt();
        if core.state() == ThreadState::Exited {
            return WakeResult::Exited;
        }
        let Some(_activity) = core.try_scheduler_activity() else {
            return WakeResult::Exited;
        };
        if core.state() == ThreadState::Parking {
            // The park winner is the atomic wake publication itself. It must
            // not acquire p->pi_lock and then wait for rq while the owner is in
            // the rq-locked block transaction. The owner rechecks this bit
            // while holding the task control lock before committing Blocked.
            return if core.publish_wake() {
                WakeResult::AlreadyPending
            } else {
                WakeResult::Notified
            };
        }
        let mut sched = core.sched().lock();
        if sched.lifecycle.state() == ThreadState::Exited {
            return WakeResult::Exited;
        }
        // Serialize publication with lifecycle and placement just as Linux
        // serializes try_to_wake_up() with p->pi_lock. A failed target lookup
        // may clear only the wake owned by this transaction; a concurrent
        // waker cannot observe and coalesce with it until that decision ends.
        if core.publish_wake() {
            return WakeResult::AlreadyPending;
        }
        if matches!(
            sched.lifecycle.state(),
            ThreadState::Ready | ThreadState::Running | ThreadState::Waking
        ) {
            // Match Linux's runnable/current fast path: the task is already
            // eligible or completing an earlier wake, so no activation and
            // no rq ownership transfer is legal. Keep the notification for
            // the next prepare_park() instead of dereferencing task-owned
            // policy state which a Ready task has transferred to its rq.
            return WakeResult::Notified;
        }
        let preferred = preferred
            .or_else(|| sched.placement.assigned_cpu())
            .or_else(|| core.wake_cpu_hint());
        if sched.lifecycle.state() == ThreadState::Blocked && sched.placement.on_cpu().is_some() {
            match Self::consume_wake_locked(&core, &mut sched) {
                Ok(WakeTransition::DeferredUntilSwitchTail) => return WakeResult::Notified,
                Ok(WakeTransition::Notified | WakeTransition::Activate) | Err(_) => {
                    task_runtime::fatal_invariant(0x574b_0003, core.id().as_u64() as usize)
                }
            }
        }
        let policy = sched.policy.active().policy();
        let queued_entity = sched.policy.active().entity().clone();
        let target = self.select_wake_target(&sched, policy, queued_entity, preferred);
        let Some(target) = target else {
            core.discard_failed_wake();
            return WakeResult::Unavailable;
        };
        let Some(publication) = self.cpu_remotes[target.as_usize()].begin_publication() else {
            core.discard_failed_wake();
            return WakeResult::Unavailable;
        };
        let transition = match Self::consume_wake_locked(&core, &mut sched) {
            Ok(transition) => transition,
            Err(_) => task_runtime::fatal_invariant(0x574b_0002, core.id().as_u64() as usize),
        };
        match transition {
            WakeTransition::Notified => WakeResult::Notified,
            WakeTransition::Activate => {
                self.activate_waking_thread_locked(&core, sched, target, publication)
            }
            WakeTransition::DeferredUntilSwitchTail => {
                task_runtime::fatal_invariant(0x574b_0004, core.id().as_u64() as usize)
            }
        }
    }

    fn activate_waking_thread_locked(
        &self,
        core: &Arc<ThreadCore>,
        mut sched: crate::lock::IrqTicketGuard<'_, ThreadSchedState>,
        target: CpuId,
        publication: CpuRemotePublication<'_>,
    ) -> WakeResult {
        if sched.lifecycle.state() != ThreadState::Waking || sched.placement.on_cpu().is_some() {
            task_runtime::fatal_invariant(0x574b_0005, core.id().as_u64() as usize);
        }
        if sched.transition(core, ThreadState::Ready).is_err() {
            task_runtime::fatal_invariant(0x574b_0006, core.id().as_u64() as usize);
        }
        #[cfg(feature = "qperf-metrics")]
        crate::metrics::record_direct_wake_activation();

        let remote = &self.cpu_remotes[target.as_usize()];
        remote.cancel_idle_pull_if_uncommitted();
        if let Some(source) = sched
            .deadline
            .bandwidth
            .reservation_owner()
            .filter(|source| *source != target)
        {
            let source_remote = &self.cpu_remotes[source.as_usize()];
            let mut source_run_queue = OwnerRqTxn::begin(self, source_remote);
            Self::detach_owner_deadline_bandwidth_in_rq(
                core,
                &mut sched,
                source_remote,
                &mut source_run_queue,
            );
            source_run_queue.commit();
            // The old physical clockevent may still point at the cancelled
            // inactive/CBS timer. Its owner recomputes the base before idle;
            // a racing stale edge is harmless and will be stopped by the
            // clockevent firing transaction.
            source_remote.request_scheduler_work();
            source_remote.kick_scheduler_work();
        }
        let mut run_queue = OwnerRqTxn::begin(self, remote);
        let now_ns = run_queue.clock().wall().as_nanos();
        let policy = sched.policy.active().policy();
        let mut queued_entity = sched.policy.active().entity().clone();
        let deadline_wake = matches!(policy, SchedulePolicy::Deadline(_)) && !sched.is_pi_boosted();
        if deadline_wake {
            queued_entity.activate_deadline(now_ns);
            *sched.policy.active_mut().entity_mut() = queued_entity.clone();
        }
        Self::activate_deadline_bandwidth_locked(core, &mut sched, &mut run_queue, target);
        if deadline_wake
            && queued_entity
                .deadline()
                .is_some_and(DeadlineEntity::is_throttled)
        {
            self.link_owner_throttled_deadline_locked(&mut run_queue, core, &mut sched, target);
            run_queue.commit();
            drop(sched);
            self.publish_owner_deadline_refresh_reserved(core, target, publication);
            return WakeResult::Notified;
        }
        let current_fair = run_queue
            .current_scheduling_entity()
            .and_then(|entity| entity.fair());
        run_queue.update_fair_virtual_time(current_fair);
        let metadata = sched.rq_task_metadata().unwrap_or_else(|_| {
            task_runtime::fatal_invariant(0x574b_0103, core.id().as_u64() as usize)
        });
        let active = sched.policy.take_active();
        debug_assert_eq!(active.policy(), policy);
        debug_assert_eq!(active.entity(), &queued_entity);
        let queued_entity = run_queue.enqueue_task(
            QueuedThread::new(
                core.id(),
                active,
                Arc::clone(core),
                sched.is_pi_boosted_rt_owner_for(policy),
                sched.affinity.affinity.is_migration_capable(),
                metadata,
            ),
            EnqueueReason::Wake,
            current_fair,
        );
        run_queue.update_fair_virtual_time(current_fair);
        let fair_virtual_time = queued_entity
            .fair()
            .map_or(0, |fair| run_queue.virtual_time_for_mode(fair.mode()));
        let preemption =
            run_queue.wakeup_preempt(core.id(), policy, queued_entity.clone(), fair_virtual_time);
        let preempts_current = preemption.requests_reschedule();
        core.publish_effective_schedule(policy, &queued_entity);
        sched.placement.activate(target);
        core.set_wake_cpu_hint(target);
        let rt_deadline_push_pending = self.rt_deadline_push_pending(remote);
        run_queue.commit();
        drop(sched);
        let rt_period_started = policy.rt_priority().is_some()
            && self
                .root_domain
                .activate_rt_period(target, task_runtime::monotonic_now());

        #[cfg(feature = "qperf-metrics")]
        crate::metrics::record_direct_wake_enqueue();
        #[cfg(feature = "qperf-metrics")]
        if preempts_current {
            crate::metrics::record_direct_wake_preemption();
        }
        #[cfg(feature = "qperf-metrics")]
        match preemption {
            WakePreemptionDecision::KeepCurrent => {
                crate::metrics::record_direct_wake_current_kept()
            }
            WakePreemptionDecision::QueuedCandidateSelected => {
                crate::metrics::record_direct_wake_queued_candidate_selected()
            }
            WakePreemptionDecision::WakeeSelected => {}
        }
        if deadline_wake {
            if preempts_current {
                remote.request_reschedule();
            }
            self.publish_owner_deadline_refresh_reserved(core, target, publication);
        } else {
            drop(publication);
            if preempts_current {
                remote.request_remote_reschedule();
            }
        }
        if rt_deadline_push_pending && !preempts_current {
            // Linux queues the RT/DL push balance callback in the enqueue
            // transaction. The target owner performs migration after dropping
            // the wakee's rq lock and revalidates the pushable candidate.
            remote.kick_scheduler_work();
        }
        if rt_period_started {
            remote.kick_scheduler_work();
        }
        WakeResult::Notified
    }

    /// Completes Linux's `TASK_WAKING` handoff after `finish_task()`.
    pub(super) fn finish_switch_tail_wake(&self, core: &Arc<ThreadCore>) {
        let sched = core.sched().lock();
        if sched.lifecycle.state() != ThreadState::Waking {
            return;
        }
        if sched.placement.on_cpu().is_some()
            || sched.placement.queued_cpu().is_some()
            || sched.placement.committed_migration_target().is_some()
        {
            task_runtime::fatal_invariant(0x574b_0007, core.id().as_u64() as usize);
        }
        let policy = sched.policy.active().policy();
        let entity = sched.policy.active().entity().clone();
        let preferred = sched
            .placement
            .assigned_cpu()
            .or_else(|| core.wake_cpu_hint());
        let target = self
            .select_wake_target(&sched, policy, entity, preferred)
            .unwrap_or_else(|| {
                task_runtime::fatal_invariant(0x574b_0008, core.id().as_u64() as usize)
            });
        let publication = self.cpu_remotes[target.as_usize()]
            .begin_publication()
            .unwrap_or_else(|| {
                task_runtime::fatal_invariant(0x574b_0009, core.id().as_u64() as usize)
            });
        let _result = self.activate_waking_thread_locked(core, sched, target, publication);
    }

    pub(super) fn enqueue_owner_thread(
        &self,
        mut cpu: Pin<&mut CpuLocal>,
        core: Arc<ThreadCore>,
        reason: EnqueueReason,
    ) -> Result<(), TaskError> {
        self.ensure_owner_cpu_online(&cpu)?;
        let mut sched = core.sched().lock();
        let preempts_current =
            self.enqueue_owner_thread_locked(cpu.as_mut(), &core, &mut sched, reason)?;
        let affinity_completed = Self::complete_affinity_if_satisfied_locked(&core, &sched);
        drop(sched);
        if affinity_completed {
            core.notify_affinity_waiters();
        }
        self.finish_owner_enqueue(cpu, reason, preempts_current);
        Ok(())
    }

    pub(super) fn enqueue_owner_thread_locked(
        &self,
        mut cpu: Pin<&mut CpuLocal>,
        core: &Arc<ThreadCore>,
        sched: &mut ThreadSchedState,
        reason: EnqueueReason,
    ) -> Result<bool, TaskError> {
        let owner = cpu.owner();
        if sched.lifecycle.state() != ThreadState::Ready {
            return Err(TaskError::NotReady);
        }
        if !sched.affinity.affinity.contains(owner) && !matches!(reason, EnqueueReason::Migrated) {
            return Err(TaskError::InvalidCpu(owner.as_u32()));
        }
        cpu.as_ref()
            .get_ref()
            .remote()
            .cancel_idle_pull_if_uncommitted();
        let remote = Arc::clone(cpu.remote());
        let mut transaction = OwnerRqTxn::begin(self, &remote);
        let now_ns = transaction.clock().wall().as_nanos();
        let policy = sched.policy.active().policy();
        let mut queued_entity = sched.policy.active().entity().clone();
        if matches!(reason, EnqueueReason::Wake)
            && matches!(policy, SchedulePolicy::Deadline(_))
            && !sched.is_pi_boosted()
        {
            queued_entity.activate_deadline(now_ns);
            *sched.policy.active_mut().entity_mut() = queued_entity.clone();
        }
        let deadline_wake_throttled = queued_entity
            .deadline()
            .is_some_and(DeadlineEntity::is_throttled);
        if deadline_wake_throttled {
            self.link_owner_throttled_deadline_locked(&mut transaction, core, sched, owner);
            let preempts_current = self
                .refresh_owner_deadline_timers_in_rq(
                    core,
                    sched,
                    cpu.as_mut(),
                    now_ns,
                    &mut transaction,
                )
                .unwrap_or(false);
            transaction.commit();
            return Ok(preempts_current);
        }
        let preempts_current =
            self.link_owner_ready_thread_locked(owner, &mut transaction, core, sched, reason);
        let timer_preempts = self
            .refresh_owner_deadline_timers_in_rq(core, sched, cpu, now_ns, &mut transaction)
            .unwrap_or(false);
        transaction.commit();
        Ok(preempts_current || timer_preempts)
    }

    fn link_owner_throttled_deadline_locked(
        &self,
        run_queue: &mut OwnerRqTxn<'_>,
        core: &Arc<ThreadCore>,
        sched: &mut ThreadSchedState,
        owner: CpuId,
    ) {
        let policy = sched.policy.active().policy();
        let entity = sched.policy.active().entity().clone();
        if !matches!(policy, SchedulePolicy::Deadline(_)) || !entity.is_deadline_throttled() {
            task_runtime::fatal_invariant(0x574b_1110, core.id().as_u64() as usize);
        }
        Self::activate_deadline_bandwidth_locked(core, sched, run_queue, owner);
        let metadata = sched.rq_task_metadata().unwrap_or_else(|_| {
            task_runtime::fatal_invariant(0x574b_1111, core.id().as_u64() as usize)
        });
        let active = sched.policy.take_active();
        run_queue.enqueue_throttled_deadline(QueuedThread::new(
            core.id(),
            active,
            Arc::clone(core),
            false,
            sched.affinity.affinity.is_migration_capable(),
            metadata,
        ));
        sched.placement.activate(owner);
        core.publish_effective_schedule(policy, &entity);
        core.set_wake_cpu_hint(owner);
    }

    pub(super) fn link_owner_ready_thread_locked(
        &self,
        owner: CpuId,
        run_queue: &mut OwnerRqTxn<'_>,
        core: &Arc<ThreadCore>,
        sched: &mut ThreadSchedState,
        reason: EnqueueReason,
    ) -> bool {
        let policy = sched.policy.active().policy();
        let current_fair = run_queue
            .current_scheduling_entity()
            .and_then(|entity| entity.fair());
        run_queue.update_fair_virtual_time(current_fair);
        let metadata = sched.rq_task_metadata().unwrap_or_else(|_| {
            task_runtime::fatal_invariant(0x574b_1102, core.id().as_u64() as usize)
        });
        let active = sched.policy.take_active();
        let queued_entity = run_queue.enqueue_task(
            QueuedThread::new(
                core.id(),
                active,
                Arc::clone(core),
                sched.is_pi_boosted_rt_owner_for(policy),
                sched.affinity.affinity.is_migration_capable(),
                metadata,
            ),
            reason,
            current_fair,
        );
        Self::activate_deadline_bandwidth_locked(core, sched, run_queue, owner);
        run_queue.update_fair_virtual_time(current_fair);
        let fair_virtual_time = queued_entity
            .fair()
            .map_or(0, |fair| run_queue.virtual_time_for_mode(fair.mode()));
        let preempts_current = run_queue
            .wakeup_preempt(core.id(), policy, queued_entity.clone(), fair_virtual_time)
            .requests_reschedule();
        core.publish_effective_schedule(policy, &queued_entity);
        if sched.placement.on_cpu() == Some(owner) {
            // Fair removes current from its class tree while Linux keeps the
            // task logically on_rq. Re-linking it is put_prev, not activation.
            sched.placement.put_prev(owner);
        } else {
            sched.placement.activate(owner);
        }
        core.set_wake_cpu_hint(owner);
        preempts_current
    }

    pub(super) fn finish_owner_enqueue(
        &self,
        cpu: Pin<&mut CpuLocal>,
        reason: EnqueueReason,
        preempts_current: bool,
    ) {
        if matches!(
            reason,
            EnqueueReason::Wake | EnqueueReason::Replenished | EnqueueReason::Migrated
        ) && preempts_current
        {
            cpu.request_reschedule();
        }
        if cpu.lock_run_queue().has_runnable_rt() {
            self.root_domain
                .activate_rt_period(cpu.owner(), task_runtime::monotonic_now());
        }
        if !preempts_current && self.rt_deadline_push_pending(cpu.remote()) {
            cpu.remote().kick_scheduler_work();
        }
    }

    pub(super) fn activate_owner_deadline_bandwidth(
        &self,
        core: &Arc<ThreadCore>,
        sched: &mut ThreadSchedState,
        cpu: Pin<&mut CpuLocal>,
        owner: CpuId,
    ) {
        let remote = Arc::clone(cpu.remote());
        let mut transaction = OwnerRqTxn::begin(self, &remote);
        Self::activate_deadline_bandwidth_locked(core, sched, &mut transaction, owner);
        transaction.commit();
    }

    pub(super) fn activate_deadline_bandwidth_locked(
        core: &Arc<ThreadCore>,
        sched: &mut ThreadSchedState,
        run_queue: &mut OwnerRqTxn<'_>,
        owner: CpuId,
    ) {
        if !matches!(sched.policy.base, SchedulePolicy::Deadline(_)) {
            return;
        }
        match sched.deadline.bandwidth.reservation_owner() {
            None => Self::attach_deadline_bandwidth_locked(core, sched, run_queue, owner, true),
            Some(assigned) if assigned != owner => {
                task_runtime::fatal_invariant(0x444c_000a, core.id().as_u64() as usize)
            }
            Some(_) if !sched.deadline.bandwidth.is_active() => {
                run_queue.activate_deadline_bandwidth(sched.deadline.bandwidth.reservation_scaled())
            }
            Some(_) => {}
        }
        sched.deadline.bandwidth.activate_contending();
    }

    /// Attaches one admitted DL reservation to a new rq without changing its
    /// contending state. Linux uses this form for inactive-timer/hotplug
    /// migration, where `this_bw` always moves and `running_bw` moves only for
    /// a still-active reservation.
    pub(super) fn attach_deadline_bandwidth_locked(
        core: &Arc<ThreadCore>,
        sched: &mut ThreadSchedState,
        run_queue: &mut OwnerRqTxn<'_>,
        owner: CpuId,
        active: bool,
    ) {
        if sched.deadline.bandwidth.reservation_owner().is_some() {
            task_runtime::fatal_invariant(0x444c_0010, core.id().as_u64() as usize);
        }
        run_queue.register_deadline_member(core);
        run_queue.add_deadline_bandwidth(sched.deadline.bandwidth.reservation_scaled(), active);
        sched.deadline.bandwidth.attach(owner);
    }

    pub(super) fn detach_owner_deadline_bandwidth_in_rq(
        core: &Arc<ThreadCore>,
        sched: &mut ThreadSchedState,
        remote: &CpuRemote,
        run_queue: &mut OwnerRqTxn<'_>,
    ) {
        let owner = remote.owner();
        let Some(assigned_cpu) = sched.deadline.bandwidth.reservation_owner() else {
            return;
        };
        if assigned_cpu != owner {
            task_runtime::fatal_invariant(0x444c_000b, core.id().as_u64() as usize);
        }
        let bandwidth = run_queue.deadline_bandwidth();
        let reservation_scaled = sched.deadline.bandwidth.reservation_scaled();
        if bandwidth.this_bw_scaled() < reservation_scaled
            || (sched.deadline.bandwidth.is_active()
                && bandwidth.running_bw_scaled() < reservation_scaled)
        {
            task_runtime::fatal_invariant(0x444c_000c, core.id().as_u64() as usize);
        }
        Self::cancel_owner_deadline_timers_locked(core, sched, remote);
        run_queue
            .remove_deadline_bandwidth(reservation_scaled, sched.deadline.bandwidth.is_active());
        sched.deadline.bandwidth.detach(owner);
        run_queue.unregister_deadline_member(core);
    }

    pub(super) fn mark_owner_deadline_non_contending_in_rq(
        &self,
        core: &Arc<ThreadCore>,
        sched: &mut ThreadSchedState,
        mut cpu: Pin<&mut CpuLocal>,
        now_ns: u64,
        run_queue: &mut OwnerRqTxn<'_>,
    ) {
        let owner = cpu.owner();
        let base_entity = if let Some(active) = sched.policy.active_option() {
            active.base_entity().clone()
        } else {
            run_queue
                .base_scheduling_entity(core.id())
                .unwrap_or_else(|| {
                    task_runtime::fatal_invariant(0x444c_1110, core.id().as_u64() as usize)
                })
        };
        let (Some(assigned_cpu), Some(deadline)) = (
            sched.deadline.bandwidth.reservation_owner(),
            base_entity.deadline(),
        ) else {
            return;
        };
        if assigned_cpu != owner || !sched.deadline.bandwidth.is_contending() {
            return;
        }
        let zero_lag = deadline_zero_lag(deadline);
        let deactivate_now = zero_lag.is_reached_by(SchedulerTimestamp::from_nanos(now_ns));
        if deactivate_now
            && run_queue.deadline_bandwidth().running_bw_scaled()
                < sched.deadline.bandwidth.reservation_scaled()
        {
            task_runtime::fatal_invariant(0x444c_000e, core.id().as_u64() as usize);
        }
        if deactivate_now {
            sched.deadline.bandwidth.deactivate();
        } else {
            sched.deadline.bandwidth.mark_non_contending(zero_lag);
        }
        if self
            .refresh_owner_deadline_timers_in_rq(core, sched, cpu.as_mut(), now_ns, run_queue)
            .is_some()
        {
            cpu.request_scheduler_work();
        }
        if deactivate_now {
            run_queue.deactivate_deadline_bandwidth(sched.deadline.bandwidth.reservation_scaled());
        }
    }

    pub(super) fn owner_dispatch_from_rq(
        core: &Arc<ThreadCore>,
        schedule: CurrentClassState,
        metadata: RqTaskMetadata,
        rt_quota_exempt: bool,
        task_now: RqTaskTime,
    ) -> CurrentDispatch {
        #[cfg(test)]
        OWNER_DISPATCH_CONSTRUCTIONS.set(OWNER_DISPATCH_CONSTRUCTIONS.get().saturating_add(1));
        CurrentDispatch::new(
            CurrentDispatchState {
                thread: core.id(),
                schedule,
                deadline_donor: metadata.deadline_donor,
                rt_quota_exempt,
                deadline_bandwidth_scaled: metadata.deadline_bandwidth_scaled,
                policy_generation: metadata.policy_generation,
                runtime_binding: metadata.runtime_binding,
            },
            core,
            task_now,
        )
    }

    pub(super) fn commit_owner_current_dispatch_in_rq(
        &self,
        transaction: &mut OwnerRqTxn<'_>,
    ) -> OwnerDispatchCommit {
        if transaction.current().is_none() {
            return OwnerDispatchCommit::NONE;
        }
        let current = transaction.current_thread();
        let current_core = transaction.current_core();
        let _charge = transaction.settle_current(0);
        let task_now_ns = transaction.clock().task().as_nanos();
        let Some(mut dispatch) = transaction.take_current() else {
            task_runtime::fatal_invariant(0x5251_1101, transaction.owner().as_u32() as usize);
        };
        if current != Some(dispatch.thread())
            || current_core.is_none_or(|core| !Arc::ptr_eq(&core, dispatch.runtime_core_arc()))
        {
            task_runtime::fatal_invariant(0x5251_1102, dispatch.thread().as_u64() as usize);
        }
        dispatch.finish_runtime_accounting(task_now_ns);
        let overrun_work = Self::sync_runtime_dispatch_state(&mut dispatch);
        transaction.install_current(dispatch);
        OwnerDispatchCommit { overrun_work }
    }

    pub(super) fn finish_owner_dispatch_commit(
        &self,
        _cpu: Pin<&mut CpuLocal>,
        commit: OwnerDispatchCommit,
        _wall_now_ns: u64,
    ) {
        if let Some(core) = commit.overrun_work {
            let mut sched = core.sched().lock();
            sched.deadline.overrun_events = sched
                .deadline
                .overrun_events
                .checked_add(1)
                .unwrap_or_else(|| {
                    task_runtime::fatal_invariant(0x5251_1103, core.id().as_u64() as usize)
                });
            drop(sched);
            self.publish_deadline_overrun_work(core);
        }
    }

    pub(super) fn sync_owner_current_dispatch_in_rq(
        &self,
        transaction: &mut OwnerRqTxn<'_>,
    ) -> Option<Arc<ThreadCore>> {
        let current = transaction.current_thread();
        let current_core = transaction.current_core();
        let dispatch = transaction.current_mut()?;
        if current != Some(dispatch.thread())
            || current_core
                .as_ref()
                .is_none_or(|core| !Arc::ptr_eq(core, dispatch.runtime_core_arc()))
        {
            task_runtime::fatal_invariant(0x5251_1104, dispatch.thread().as_u64() as usize);
        }
        Self::sync_runtime_dispatch_state(dispatch)
    }

    fn sync_runtime_dispatch_state(dispatch: &mut CurrentDispatch) -> Option<Arc<ThreadCore>> {
        let _charged_runtime_ns = dispatch.take_charged_runtime_ns();
        let overrun_core = dispatch.deadline_overrun_core();
        dispatch.take_deadline_overrun().then_some(overrun_core)
    }

    pub(super) fn apply_policy_generation_locked(
        &self,
        sched: &mut ThreadSchedState,
        active: &mut ActiveSchedulingState,
        generation: u64,
        owner_now_ns: Option<u64>,
        fair_placement: Option<FairPolicyPlacement>,
        activate_deadline: bool,
    ) -> Result<Option<PolicyGenerationCommit>, TaskError> {
        Self::validate_owner_policy_generation(sched, generation)?;
        let Some(pending) = sched.policy.pending_update() else {
            return Ok(None);
        };
        let base_policy = pending.policy;
        let previous_base_entity = active.base_entity().clone();
        let mut base_entity = match (previous_base_entity, base_policy) {
            (SchedulingEntity::Fair(fair), SchedulePolicy::Fair { nice, mode }) => {
                let source_virtual_time = fair_placement
                    .map(|placement| placement.source_virtual_time)
                    .unwrap_or_else(|| fair.vruntime());
                let destination_virtual_time = fair_placement
                    .map(|placement| placement.destination_virtual_time)
                    .unwrap_or(source_virtual_time);
                SchedulingEntity::Fair(fair.reconfigure(
                    nice,
                    mode,
                    source_virtual_time,
                    destination_virtual_time,
                ))
            }
            _ => SchedulingEntity::new_with_deadline_server(
                base_policy,
                self.config.fair_slice_ns(),
                fair_placement.map_or(0, |placement| placement.destination_virtual_time),
                sched.deadline.server.clone(),
            ),
        };
        if activate_deadline {
            let now_ns = owner_now_ns.ok_or(TaskError::InvalidConfiguration)?;
            base_entity.activate_deadline(now_ns);
        }
        let next_dispatch_generation = sched
            .policy
            .dispatch_generation
            .checked_add(1)
            .ok_or(TaskError::InvalidConfiguration)?;
        active.replace_base_entity(base_entity);
        if sched.pi.donor.is_none() {
            active.use_base_entity(base_policy);
        }
        let held_deadline_reservation = sched.held_deadline_reservation();
        let committed = sched.policy.commit_pending_update();
        debug_assert_eq!(committed, pending);
        sched
            .deadline
            .bandwidth
            .replace_detached_reservation(committed.reservation_scaled);
        sched.policy.dispatch_generation = next_dispatch_generation;
        let running_policy_changed = sched.placement.execution_cpu().is_some();
        Ok(Some(PolicyGenerationCommit {
            base_policy,
            running_policy_changed,
            held_deadline_reservation,
            committed_deadline_reservation: committed.reservation_scaled,
        }))
    }

    pub(super) fn finish_policy_admission_locked(
        root_domain: &mut root_domain::RootDomainGuard<'_>,
        core: &Arc<ThreadCore>,
        commit: PolicyGenerationCommit,
    ) {
        root_domain
            .replace_deadline_utilization(
                commit.held_deadline_reservation,
                commit.committed_deadline_reservation,
            )
            .unwrap_or_else(|_| {
                task_runtime::fatal_invariant(0x444c_1202, core.id().as_u64() as usize)
            });
    }

    pub(super) fn notify_policy_generation(
        core: &Arc<ThreadCore>,
        commit: PolicyGenerationCommit,
        owner_now_ns: Option<u64>,
    ) {
        if commit.running_policy_changed
            && let Some(extension) = core.extension_view()
        {
            let now_ns = owner_now_ns.unwrap_or_else(|| {
                task_runtime::fatal_invariant(0x5251_1301, core.id().as_u64() as usize)
            });
            // SAFETY: the thread-state lock is released. A running update
            // executes on the placement owner while it retains the scheduler
            // baton. Construction guarantees that the callback is bounded and
            // valid for this retained ThreadCore.
            unsafe {
                extension.notify_running_policy_applied(core.id(), commit.base_policy, now_ns)
            };
        }
    }

    pub(super) fn validate_owner_policy_generation(
        sched: &ThreadSchedState,
        generation: u64,
    ) -> Result<(), TaskError> {
        let pending = sched
            .policy
            .pending_update()
            .ok_or(TaskError::InvalidConfiguration)?;
        if generation != pending.generation || generation != sched.policy.update_generation() {
            return Err(TaskError::InvalidConfiguration);
        }
        sched
            .policy
            .dispatch_generation
            .checked_add(1)
            .ok_or(TaskError::InvalidConfiguration)?;
        Ok(())
    }

    pub(super) fn recompute_pi_after_policy_update(
        &self,
        thread: ThreadId,
    ) -> Result<(), TaskError> {
        self.propagate_pi_waiter_key_after_policy_change(thread)
    }
}
