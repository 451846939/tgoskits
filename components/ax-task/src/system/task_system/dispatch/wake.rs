//! Direct wakeup and owner-runqueue activation transactions.

use super::*;

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
    pub(in crate::system::task_system) fn finish_switch_tail_wake(&self, core: &Arc<ThreadCore>) {
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

    pub(in crate::system::task_system) fn enqueue_owner_thread(
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

    pub(in crate::system::task_system) fn enqueue_owner_thread_locked(
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
}
