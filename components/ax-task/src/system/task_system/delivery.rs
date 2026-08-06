use super::*;

impl TaskSystem {
    /// Enqueues a ready thread on an affinity-compatible owner CPU.
    pub fn enqueue(&self, mut cpu: Pin<&mut CpuLocal>, thread: ThreadId) -> Result<(), TaskError> {
        self.ensure_owner_cpu_context(&cpu)?;
        let core = {
            let state = self.state.lock();
            state.ensure_cpu_online(&cpu)?;
            Arc::clone(&state.thread_record(thread)?.core)
        };
        self.enqueue_owner_thread(cpu.as_mut(), core, EnqueueReason::Wake)?;
        Self::program_local_timer(cpu.as_mut())
    }

    /// Places a newly ready thread on an allowed active CPU.
    ///
    /// Ordinary fair work is placed on the least-loaded allowed CPU, including
    /// its current non-idle dispatch and migrations not yet consumed by the
    /// destination owner. Other classes preserve owner-local placement unless
    /// affinity requires a transfer. Remote placement uses the owner-only
    /// owner-control inbox and never mutates another CPU's runqueue.
    ///
    /// # Errors
    ///
    /// Returns an error when the source CPU is offline, the thread is not a
    /// unique unqueued Ready thread, no allowed CPU is online, or local timer
    /// programming fails.
    pub fn place_ready(
        &self,
        mut cpu: Pin<&mut CpuLocal>,
        thread: ThreadId,
    ) -> Result<(), TaskError> {
        self.ensure_owner_cpu_context(&cpu)?;
        let owner = cpu.owner();
        let migration = {
            let state = self.state.lock();
            state.ensure_cpu_online(&cpu)?;
            let record = state.thread_record(thread)?;
            let mut sched = record.sched.lock();
            if sched.lifecycle.state() != ThreadState::Ready {
                return Err(TaskError::NotReady);
            }
            if sched.placement.queued_cpu().is_some()
                || sched.placement.execution_cpu().is_some()
                || sched.placement.on_cpu().is_some()
                || sched.placement.has_pending_migration()
            {
                return Err(TaskError::AlreadyQueued);
            }
            let affinity = &sched.placement.affinity;
            let policy = sched.policy.effective;
            let load_aware = matches!(
                policy,
                SchedulePolicy::Fair {
                    mode: FairMode::Normal | FairMode::Batch,
                    ..
                }
            );
            let target = if load_aware {
                state.select_initial_fair_cpu(affinity, owner)
            } else if matches!(
                policy,
                SchedulePolicy::Fifo { .. }
                    | SchedulePolicy::RoundRobin { .. }
                    | SchedulePolicy::Deadline(_)
            ) {
                self.select_priority_cpu(
                    policy,
                    sched.policy.effective_entity,
                    affinity,
                    Some(owner),
                    None,
                )
            } else if affinity.contains(owner) {
                Some(owner)
            } else {
                state.select_allowed_cpu(affinity)
            }
            .ok_or(TaskError::InvalidConfiguration)?;
            let core = Arc::clone(&record.core);
            if target == owner {
                drop(sched);
                drop(state);
                self.enqueue_owner_thread(cpu.as_mut(), core, EnqueueReason::Wake)?;
                None
            } else {
                let carrier = self.prepare_owner_migration(&core, owner, target)?;
                sched.placement.begin_remote_wakeup(target);
                record.core.set_wake_cpu_hint(target);
                drop(sched);
                Some((carrier, target))
            }
        };
        if let Some((carrier, target)) = migration {
            #[cfg(not(test))]
            let _ = target;
            #[cfg(test)]
            placement::inject_migration_publication_race(self, target);
            carrier.commit();
            return Ok(());
        }
        Self::program_local_timer(cpu.as_mut())
    }

    /// Removes a ready thread from its owner run queue for migration or update.
    pub fn dequeue(&self, mut cpu: Pin<&mut CpuLocal>, thread: ThreadId) -> Result<(), TaskError> {
        self.ensure_owner_cpu_context(&cpu)?;
        let state = self.state.lock();
        state.ensure_cpu_online(&cpu)?;
        let record = state.thread_record(thread)?;
        let mut sched = record.sched.lock();
        let queued = cpu
            .lock_run_queue()
            .dequeue(thread)
            .ok_or(TaskError::NotReady)?;
        sched.policy.effective_entity = queued.entity;
        if !sched.is_pi_boosted() {
            sched.policy.base_entity = queued.entity;
        }
        sched.placement.deactivate(cpu.owner());
        drop(sched);
        drop(state);
        self.publish_owner_cpu_load_summary(cpu.as_mut());
        Ok(())
    }

    /// Reconciles task metadata written by a remote affinity setter with the
    /// physical placement owned by this CPU.
    ///
    /// The affinity mask may be updated under the stable thread lock from any
    /// CPU. Runqueue membership and switch-tail state are different: only the
    /// CPU named by the placement state may mutate them. This is the local
    /// equivalent of Linux taking a task's `pi_lock` together with its owning
    /// runqueue lock before moving a queued task.
    fn reconcile_owner_affinity_update(
        &self,
        mut cpu: Pin<&mut CpuLocal>,
        core: &Arc<ThreadCore>,
    ) -> Result<(), TaskError> {
        let owner = cpu.owner();
        let mut sched = core.sched().lock();
        let execution_cpu = sched.placement.execution_cpu();
        let queued_cpu = sched.placement.queued_cpu();
        let on_cpu = sched.placement.on_cpu();
        let migration_target = sched.placement.committed_migration_target();
        let physical_owner = execution_cpu
            .or(queued_cpu)
            .or(on_cpu)
            .or(migration_target)
            .or(sched.deadline.bandwidth_cpu);
        let target = if sched.placement.affinity.contains(owner) {
            owner
        } else {
            self.select_allowed_active_cpu(&sched.placement.affinity, Some(owner))
                .ok_or(TaskError::InvalidConfiguration)?
        };
        core.set_wake_cpu_hint(target);

        if let Some(physical_owner) = physical_owner
            && physical_owner != owner
        {
            drop(sched);
            return self.publish_owner_affinity_retry(core, physical_owner, target);
        }

        // Owner-control draining is forbidden while a switch handoff exists,
        // so an outgoing-only `on_cpu` owner here indicates corrupt placement
        // state rather than work that can be made safe by self-republication.
        if on_cpu == Some(owner) && execution_cpu.is_none() {
            return Err(TaskError::InvalidConfiguration);
        }

        if queued_cpu == Some(owner) && on_cpu.is_none() {
            if !cpu.lock_run_queue().update_migration_capability(
                core.id(),
                sched.placement.affinity.is_migration_capable(),
            ) {
                return Err(TaskError::InvalidConfiguration);
            }
            if target == owner {
                sched.placement.request_migration(None);
                let completed = Self::complete_affinity_if_satisfied_locked(core, &sched);
                drop(sched);
                self.publish_owner_cpu_load_summary(cpu.as_mut());
                if completed {
                    core.notify_affinity_waiters();
                }
                return Ok(());
            }
            let carrier = self.prepare_owner_migration(core, owner, target)?;
            let detached = {
                let current_fair = cpu
                    .dispatch_state()
                    .current_dispatch
                    .as_ref()
                    .and_then(|current| current.entity.fair());
                cpu.lock_run_queue()
                    .detach_for_transfer(
                        core.id(),
                        current_fair,
                        self.config.timing_granularity_ns(),
                    )
                    .ok_or(TaskError::NotReady)?
            };
            let queued_entity = detached.entity;
            Self::detach_owner_deadline_bandwidth_locked(core, &mut sched, cpu.as_mut());
            sched.policy.effective_entity = queued_entity;
            if !sched.is_pi_boosted() {
                sched.policy.base_entity = queued_entity;
            }
            self.capture_owner_fair_migration(cpu.as_ref().get_ref(), &mut sched);
            sched.placement.begin_migration(owner, target);
            core.set_wake_cpu_hint(target);
            drop(sched);
            drop(detached);
            carrier.commit();
            self.publish_owner_cpu_load_summary(cpu.as_mut());
            return Ok(());
        }

        if execution_cpu == Some(owner) {
            if cpu.current() != Some(core.id()) {
                return Err(TaskError::InvalidConfiguration);
            }
            sched
                .placement
                .request_migration((target != owner).then_some(target));
            let completed = Self::complete_affinity_if_satisfied_locked(core, &sched);
            drop(sched);
            if completed {
                core.notify_affinity_waiters();
            }
            if target != owner {
                cpu.request_reschedule();
            }
            self.publish_owner_cpu_load_summary(cpu.as_mut());
            return Ok(());
        }

        if migration_target == Some(owner) {
            sched
                .placement
                .request_migration((target != owner).then_some(target));
            return Ok(());
        }

        if sched.deadline.bandwidth_cpu == Some(owner) && target != owner {
            return Err(TaskError::InvalidConfiguration);
        }
        let completed = Self::complete_affinity_if_satisfied_locked(core, &sched);
        drop(sched);
        if completed {
            core.notify_affinity_waiters();
        }
        Ok(())
    }

    /// Applies a bounded batch of owner-CPU effective-policy updates.
    pub fn drain_policy_updates(
        &self,
        cpu: Pin<&mut CpuLocal>,
    ) -> Result<OwnerControlDrain, TaskError> {
        let now_ns = cpu.update_rq_clock().wall_nanos();
        self.drain_policy_updates_with_clock(cpu, now_ns)
    }

    pub(super) fn drain_policy_updates_with_clock(
        &self,
        mut cpu: Pin<&mut CpuLocal>,
        now_ns: u64,
    ) -> Result<OwnerControlDrain, TaskError> {
        self.ensure_owner_cpu_context(&cpu)?;
        self.ensure_owner_cpu_online(&cpu)?;
        // Owner-control work is ordered after the architecture switch tail.
        // Until then `on_cpu` is a lifetime pin for the outgoing stack, not a
        // runnable-placement owner. Consuming an affinity update in this
        // window either has to republish itself indefinitely or can lose the
        // completion when the tail detaches a blocked task. Linux closes the
        // same interval in `finish_task_switch()` before the rq owner handles
        // migration work. Keep the original intrusive publication pending and
        // make the scheduler revisit it after tail instead.
        if cpu.as_ref().get_ref().switch_handoff().is_some()
            && cpu.remote().owner_control_inbox().has_pending()
        {
            cpu.request_scheduler_work();
            return Ok(OwnerControlDrain {
                drained: 0,
                pending: true,
            });
        }
        let (drained, pending) = {
            let remote = Arc::clone(cpu.remote());
            let scratch = cpu.as_mut().drain_state_mut();
            let limit = scratch.batch_limit();
            let batch = remote
                .owner_control_inbox()
                .drain(limit, &mut scratch.owner_control_buffer);
            (batch.drained(), batch.pending())
        };
        let mut detached = [InboxMessage::EMPTY; crate::DEFAULT_BATCH_LIMIT];
        detached[..drained].copy_from_slice(&cpu.drain_state().owner_control_buffer[..drained]);
        let completed_incoming_migration_demand = detached[..drained]
            .iter()
            .filter(|message| message.operation() == InboxOperation::Migration)
            .try_fold(0_u64, |demand, message| {
                demand.checked_add(message.placement_demand())
            })
            .unwrap_or_else(|| {
                task_runtime::fatal_invariant(0x4d49_4744, cpu.owner().as_u32() as usize)
            });
        cpu.remote()
            .release_incoming_migration_demand(completed_incoming_migration_demand);
        let mut messages = DetachedOwnerMessageBatch::new(&detached[..drained]);
        while let Some(message) = messages.next() {
            let operation = message.operation();
            if operation == InboxOperation::BalanceRequest {
                let source = message
                    .source_cpu()
                    .ok_or(TaskError::InvalidConfiguration)?;
                let target = message
                    .target_cpu()
                    .ok_or(TaskError::InvalidConfiguration)?;
                if source != cpu.owner() {
                    return Err(TaskError::CpuOwnerMismatch {
                        expected: source.as_u32(),
                        actual: cpu.owner().as_u32(),
                    });
                }
                let reservation = message
                    .balance_reservation()
                    .ok_or(TaskError::InvalidConfiguration)?;
                let target_remote = self
                    .cpu_remotes
                    .get(target.as_usize())
                    .ok_or(TaskError::InvalidCpu(target.as_u32()))?;
                let Some(mut claim) = target_remote.claim_idle_pull(reservation) else {
                    continue;
                };
                if !cpu
                    .try_load_summary()
                    .is_some_and(|summary| summary.is_overloaded())
                {
                    drop(claim);
                    target_remote.kick_scheduler_work();
                    continue;
                }
                if !claim.commit() {
                    continue;
                }
                let migrated = self.transfer_owner_balance_candidate(
                    cpu.as_mut(),
                    target,
                    BalanceReason::IdlePull,
                );
                drop(claim);
                match migrated {
                    Ok(BalanceTransferOutcome::Migrated(_)) => {}
                    Ok(BalanceTransferOutcome::NoCandidate | BalanceTransferOutcome::Retry) => {
                        target_remote.kick_scheduler_work();
                    }
                    Err(error) => {
                        target_remote.kick_scheduler_work();
                        return Err(error);
                    }
                }
                continue;
            }
            if matches!(
                operation,
                InboxOperation::BalanceRequest | InboxOperation::Reclaim
            ) {
                return Err(TaskError::InvalidConfiguration);
            }
            if message.payload() == 0 {
                continue;
            }
            // SAFETY: publication transfers one Arc count in the payload and
            // this detached owner message consumes that count exactly once.
            let core = unsafe {
                Arc::from_raw(ptr::with_exposed_provenance::<ThreadCore>(
                    message.payload(),
                ))
            };
            let _delivery = core.accept_scheduler_inbox_delivery();
            if core.id() != message.thread_id() {
                continue;
            }
            let Some(_activity) = core.try_scheduler_activity() else {
                // Exit owns the transition gate and will clear any pending
                // migration target before publishing the reaper retry.
                continue;
            };
            if core.state() == ThreadState::Exited {
                continue;
            }
            let owner = cpu.owner();
            let source = message
                .source_cpu()
                .ok_or(TaskError::InvalidConfiguration)?;
            let target = message
                .target_cpu()
                .ok_or(TaskError::InvalidConfiguration)?;
            if operation == InboxOperation::DeadlineRefresh {
                if source != owner || target != owner {
                    return Err(TaskError::CpuOwnerMismatch {
                        expected: source.as_u32(),
                        actual: owner.as_u32(),
                    });
                }
                let mut sched = core.sched().lock();
                if message.generation() <= sched.pi.deadline_cbs_generation {
                    if sched.placement.queued_cpu() == Some(owner) {
                        Self::activate_owner_deadline_bandwidth(
                            &core,
                            &mut sched,
                            cpu.as_mut(),
                            owner,
                        );
                    }
                    self.refresh_owner_deadline_timers_locked(
                        &core,
                        &mut sched,
                        cpu.as_mut(),
                        now_ns,
                    )?;
                }
                continue;
            }
            if operation == InboxOperation::AffinityUpdate {
                if source != owner {
                    return Err(TaskError::CpuOwnerMismatch {
                        expected: source.as_u32(),
                        actual: owner.as_u32(),
                    });
                }
                self.reconcile_owner_affinity_update(cpu.as_mut(), &core)?;
                continue;
            }
            if operation == InboxOperation::PolicyUpdate {
                if source != owner || target != owner {
                    return Err(TaskError::CpuOwnerMismatch {
                        expected: source.as_u32(),
                        actual: owner.as_u32(),
                    });
                }
                let cleanup_deadline_member = {
                    let sched = core.sched().lock();
                    sched.deadline.cleanup_pending
                        && sched.deadline.bandwidth_cpu == Some(owner)
                        && sched.placement.queued_cpu().is_none()
                        && sched.placement.execution_cpu().is_none()
                        && sched.placement.on_cpu().is_none()
                };
                if cleanup_deadline_member {
                    Self::detach_owner_deadline_bandwidth(&core, cpu.as_mut());
                    core.sched().lock().deadline.cleanup_pending = false;
                    continue;
                }
            }
            if operation == InboxOperation::Migration {
                if target != owner {
                    return Err(TaskError::CpuOwnerMismatch {
                        expected: target.as_u32(),
                        actual: owner.as_u32(),
                    });
                }
                let needs_affinity_move = {
                    let sched = core.sched().lock();
                    if sched.lifecycle.state() != ThreadState::Ready
                        || sched.placement.committed_migration_target() != Some(owner)
                        || sched.placement.queued_cpu().is_some()
                        || sched.placement.execution_cpu().is_some()
                        || sched.placement.on_cpu().is_some()
                    {
                        return Err(TaskError::InvalidConfiguration);
                    }
                    !sched.placement.affinity.contains(owner)
                        || sched.placement.requested_migration().is_some()
                };
                self.enqueue_owner_thread(
                    cpu.as_mut(),
                    Arc::clone(&core),
                    EnqueueReason::Migrated,
                )?;
                if needs_affinity_move {
                    self.reconcile_owner_affinity_update(cpu.as_mut(), &core)?;
                }
                continue;
            }
            debug_assert_eq!(operation, InboxOperation::PolicyUpdate);
            let (queued_cpu, execution_cpu, policy_generation, cbs_borrowed) = {
                let sched = core.sched().lock();
                (
                    sched.placement.queued_cpu(),
                    sched.placement.execution_cpu(),
                    sched.policy.generation,
                    sched.pi.deadline_cbs_borrower.is_some(),
                )
            };
            if message.generation() > policy_generation {
                continue;
            }
            if cbs_borrowed {
                // The remote PI owner is the sole mutable owner of this CBS
                // entity until its next scheduler safe point. Re-publish the
                // cold-path policy update instead of replacing donor state
                // underneath an in-flight dispatch copy.
                self.publish_owner_policy_retry(&core, owner, policy_generation)?;
                cpu.request_scheduler_work();
                continue;
            }
            if queued_cpu == Some(owner) && execution_cpu.is_none() {
                if cpu.dispatch_state().current_dispatch.is_some() {
                    let _settled = cpu.as_mut().settle_current_dispatch(0)?;
                } else {
                    cpu.lock_run_queue().update_fair_virtual_time(None);
                }
                let fair_placement =
                    Self::owner_fair_policy_placement(cpu.as_ref().get_ref(), &core);
                let detached = {
                    let mut sched = core.sched().lock();
                    if sched.placement.queued_cpu() != Some(owner) {
                        return Err(TaskError::InvalidConfiguration);
                    }
                    Self::validate_owner_policy_generation(&sched, message.generation())?;
                    let detached = cpu
                        .lock_run_queue()
                        .detach_for_policy_update(core.id())
                        .ok_or(TaskError::NotReady)?;
                    Self::detach_owner_deadline_bandwidth_locked(&core, &mut sched, cpu.as_mut());
                    if !sched.is_pi_boosted() {
                        sched.policy.base_entity = detached.entity;
                        sched.policy.effective_entity = detached.entity;
                    }
                    sched.placement.deactivate(owner);
                    detached
                };
                drop(detached);
                let applied = match self.apply_policy_generation(
                    &core,
                    message.generation(),
                    Some(now_ns),
                    fair_placement,
                    true,
                ) {
                    Ok(applied) => applied,
                    Err(_) => {
                        task_runtime::fatal_invariant(0x504f_0003, core.id().as_u64() as usize)
                    }
                };
                if applied && self.recompute_pi_after_policy_update(core.id()).is_err() {
                    task_runtime::fatal_invariant(0x504f_0004, core.id().as_u64() as usize);
                }
                if self
                    .enqueue_owner_thread(
                        cpu.as_mut(),
                        Arc::clone(&core),
                        EnqueueReason::PolicyChanged,
                    )
                    .is_err()
                {
                    task_runtime::fatal_invariant(0x504f_0005, core.id().as_u64() as usize);
                }
                cpu.request_reschedule();
            } else if execution_cpu == Some(owner) && cpu.current() == Some(core.id()) {
                self.commit_owner_current_dispatch(cpu.as_mut())?;
                let fair_placement =
                    Self::owner_fair_policy_placement(cpu.as_ref().get_ref(), &core);
                {
                    let mut sched = core.sched().lock();
                    Self::validate_owner_policy_generation(&sched, message.generation())?;
                    Self::detach_owner_deadline_bandwidth_locked(&core, &mut sched, cpu.as_mut());
                }
                let applied = match self.apply_policy_generation(
                    &core,
                    message.generation(),
                    Some(now_ns),
                    fair_placement,
                    true,
                ) {
                    Ok(applied) => applied,
                    Err(_) => {
                        task_runtime::fatal_invariant(0x504f_0006, core.id().as_u64() as usize)
                    }
                };
                if applied && self.recompute_pi_after_policy_update(core.id()).is_err() {
                    task_runtime::fatal_invariant(0x504f_0007, core.id().as_u64() as usize);
                }
                let dispatch = {
                    let mut sched = core.sched().lock();
                    Self::activate_owner_deadline_bandwidth(&core, &mut sched, cpu.as_mut(), owner);
                    let dispatch =
                        Self::owner_dispatch(&core, &sched, now_ns).unwrap_or_else(|_| {
                            task_runtime::fatal_invariant(0x504f_0009, core.id().as_u64() as usize)
                        });
                    let mut run_queue = cpu.lock_run_queue();
                    if run_queue.is_linked_current(core.id()) {
                        run_queue
                            .dequeue(core.id())
                            .expect("validated retained current must remain linked");
                    }
                    if matches!(
                        sched.policy.effective,
                        SchedulePolicy::Deadline(_)
                            | SchedulePolicy::Fifo { .. }
                            | SchedulePolicy::RoundRobin { .. }
                    ) && run_queue
                        .link_running(QueuedThread::new(
                            core.id(),
                            sched.policy.effective,
                            sched.policy.effective_entity,
                            Arc::clone(&core),
                            sched.is_pi_boosted_rt_owner(),
                            sched.placement.affinity.is_migration_capable(),
                        ))
                        .is_err()
                    {
                        task_runtime::fatal_invariant(0x504f_000a, core.id().as_u64() as usize);
                    }
                    dispatch
                };
                cpu.as_mut().install_dispatch(dispatch);
                self.publish_owner_cpu_load_summary(cpu.as_mut());
                cpu.request_reschedule();
            } else {
                if core.sched().lock().deadline.bandwidth_cpu == Some(owner) {
                    Self::detach_owner_deadline_bandwidth(&core, cpu.as_mut());
                }
                let applied = self.apply_policy_generation(
                    &core,
                    message.generation(),
                    Some(now_ns),
                    None,
                    false,
                )?;
                if applied {
                    self.recompute_pi_after_policy_update(core.id())?;
                }
                self.assign_owner_inactive_deadline_bandwidth(&core, cpu.as_mut(), now_ns)?;
            }
        }
        if pending {
            cpu.request_scheduler_work();
        }
        Ok(OwnerControlDrain { drained, pending })
    }

    /// Drains one bounded batch from every inbox owned by `cpu`.
    ///
    /// Owner-control inboxes, rather than `need_resched`, are the source of
    /// truth for migration, policy, and deferred owner work. Direct wakeups
    /// have already activated the target runqueue before this safe point. A
    /// bounded owner-work remainder is assigned a fresh runtime doorbell before
    /// this safe point returns.
    pub(super) fn drain_owner_work(
        &self,
        mut cpu: Pin<&mut CpuLocal>,
        now_ns: u64,
    ) -> Result<(), TaskError> {
        let policy_pending = cpu.remote().owner_control_inbox().has_pending();
        if policy_pending {
            self.drain_policy_updates_with_clock(cpu.as_mut(), now_ns)?;
        }
        if cpu.has_remote_work() {
            cpu.defer_scheduler_work();
        }
        Ok(())
    }
}
