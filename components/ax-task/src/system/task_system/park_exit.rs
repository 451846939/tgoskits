//! Park, current-thread exit, and physical switch-tail completion.

use super::*;

#[cfg(test)]
static PARK_COMMIT_WAKE_RACE_ARMED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);
#[cfg(test)]
static PARK_COMMIT_WAKE_RACE_SYSTEM: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
static PARK_COMMIT_WAKE_RACE_THREAD: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
#[cfg(test)]
static PARK_COMMIT_WAKE_RACE_ENTERED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);
#[cfg(test)]
static PARK_COMMIT_WAKE_RACE_COMPLETED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
pub(super) fn arm_park_commit_wake_race(system: &TaskSystem, thread: ThreadId) {
    PARK_COMMIT_WAKE_RACE_ENTERED.store(false, Ordering::Release);
    PARK_COMMIT_WAKE_RACE_COMPLETED.store(false, Ordering::Release);
    assert!(
        !PARK_COMMIT_WAKE_RACE_ARMED.swap(true, Ordering::AcqRel),
        "only one deterministic park race may be armed"
    );
    PARK_COMMIT_WAKE_RACE_SYSTEM.store(
        (system as *const TaskSystem).expose_provenance(),
        Ordering::Release,
    );
    PARK_COMMIT_WAKE_RACE_THREAD.store(thread.as_u64(), Ordering::Release);
}

#[cfg(test)]
pub(super) fn park_commit_wake_race_entered() -> bool {
    PARK_COMMIT_WAKE_RACE_ENTERED.load(Ordering::Acquire)
}

#[cfg(test)]
pub(super) fn complete_park_commit_wake_race() {
    PARK_COMMIT_WAKE_RACE_COMPLETED.store(true, Ordering::Release);
}

#[cfg(test)]
fn park_commit_wake_race_hook(system: &TaskSystem, thread: ThreadId) {
    if PARK_COMMIT_WAKE_RACE_SYSTEM.load(Ordering::Acquire)
        != (system as *const TaskSystem).expose_provenance()
        || PARK_COMMIT_WAKE_RACE_THREAD.load(Ordering::Acquire) != thread.as_u64()
    {
        return;
    }
    if !PARK_COMMIT_WAKE_RACE_ARMED.swap(false, Ordering::AcqRel) {
        return;
    }
    PARK_COMMIT_WAKE_RACE_ENTERED.store(true, Ordering::Release);
    while !PARK_COMMIT_WAKE_RACE_COMPLETED.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
}

pub(crate) struct CurrentExitPermit {
    thread: ThreadId,
    scheduler_exit: OwnedThreadSchedulerExit,
}

impl CurrentExitPermit {
    pub(crate) const fn thread(&self) -> ThreadId {
        self.thread
    }

    fn seal(&mut self) {
        self.scheduler_exit.seal();
    }
}

impl TaskSystem {
    /// Publishes `PARKING` after consuming a wake-before-park notification.
    pub fn prepare_park(&self, mut cpu: Pin<&mut CpuLocal>) -> Result<ParkPrepare, TaskError> {
        self.ensure_owner_cpu_context(&cpu)?;
        self.complete_context_switch(cpu.as_mut())?;
        self.ensure_owner_cpu_online(&cpu)?;
        let core = cpu.current_core().ok_or(TaskError::NoRunnableThread)?;
        if core.take_park_notification() {
            return Ok(ParkPrepare::Notified);
        }
        let generation = core.next_park_generation()?;
        core.sched().lock().transition(core, ThreadState::Parking)?;
        Ok(ParkPrepare::Prepared(ParkTicket::new(
            core.id(),
            generation,
        )))
    }

    /// Rechecks a prepared park and either cancels it or commits schedule-out.
    pub fn commit_park(
        &self,
        cpu: Pin<&mut CpuLocal>,
        token: &mut ParkTicket,
    ) -> Result<ParkCommit, TaskError> {
        self.ensure_owner_cpu_context(&cpu)?;
        let now_ns = cpu.update_rq_clock().wall_nanos();
        self.commit_park_at(cpu, token, now_ns)
    }

    fn commit_park_at(
        &self,
        mut cpu: Pin<&mut CpuLocal>,
        token: &mut ParkTicket,
        now_ns: u64,
    ) -> Result<ParkCommit, TaskError> {
        if token.is_resolved() {
            return Err(TaskError::StaleThreadId);
        }
        self.drain_owner_work(cpu.as_mut(), now_ns)?;
        self.ensure_owner_cpu_online(&cpu)?;
        if cpu.current() != Some(token.thread()) {
            return Err(TaskError::StaleThreadId);
        }
        let previous_core = cpu
            .current_core()
            .cloned()
            .ok_or(TaskError::NoRunnableThread)?;
        let generation = previous_core.park_generation();
        if generation != token.generation() {
            return Err(TaskError::StaleThreadId);
        }
        let notified = previous_core.take_park_notification();
        if notified {
            previous_core
                .sched()
                .lock()
                .transition(&previous_core, ThreadState::Running)?;
            cpu.finish_park_preemption(true);
            token.mark_resolved();
            return Ok(ParkCommit::Notified);
        }
        let scheduler_request = cpu.as_mut().claim_scheduler_request();
        cpu.defer_park_preemption(scheduler_request.preempt_requested());
        self.commit_owner_current_dispatch(cpu.as_mut())?;
        #[cfg(test)]
        park_commit_wake_race_hook(self, previous_core.id());
        let resumed_dispatch = {
            let mut sched = previous_core.sched().lock();
            // This is the serialization edge shared with wake_thread_direct.
            // A wake that observes Parking publishes PARK_NOTIFIED while
            // holding this same lock. Rechecking and either restoring Running
            // or publishing Blocked in one transaction makes that wake the
            // unique winner instead of dropping it between two observations.
            if previous_core.take_park_notification() {
                sched.transition(&previous_core, ThreadState::Running)?;
                Some(Self::owner_dispatch(&previous_core, &sched, now_ns)?)
            } else {
                if sched.lifecycle.state() != ThreadState::Parking
                    || sched.placement.execution_cpu() != Some(cpu.owner())
                    || sched.placement.on_cpu() != Some(cpu.owner())
                {
                    return Err(TaskError::InvalidConfiguration);
                }
                // Timer replacement is the final recoverable preparation.
                // A wake cannot cross this point while the thread lock is
                // held; all following rq and placement changes are one owner
                // commit and cannot return a partial block.
                self.mark_owner_deadline_non_contending_locked(
                    &previous_core,
                    &mut sched,
                    cpu.as_mut(),
                    now_ns,
                )?;
                let timing_granularity_ns = self.config.timing_granularity_ns();
                if let Some(fair) = sched.policy.effective_entity.fair() {
                    let virtual_time = cpu.lock_run_queue().virtual_time_for_mode(fair.mode());
                    sched
                        .policy
                        .effective_entity
                        .capture_fair_sleep_lag(virtual_time, timing_granularity_ns);
                }
                if !sched.is_pi_boosted()
                    && let Some(fair) = sched.policy.base_entity.fair()
                {
                    let virtual_time = cpu.lock_run_queue().virtual_time_for_mode(fair.mode());
                    sched
                        .policy
                        .base_entity
                        .capture_fair_sleep_lag(virtual_time, timing_granularity_ns);
                }
                sched.transition(&previous_core, ThreadState::Blocked)?;
                let mut run_queue = cpu.lock_run_queue();
                if run_queue.is_linked_current(previous_core.id()) {
                    run_queue
                        .dequeue(previous_core.id())
                        .expect("validated retained current must remain linked");
                }
                sched.placement.block_current(cpu.owner());
                None
            }
        };
        if let Some(dispatch) = resumed_dispatch {
            cpu.as_mut().install_dispatch(dispatch);
            cpu.finish_park_preemption(true);
            self.publish_owner_cpu_load_summary(cpu.as_mut());
            cpu.acknowledge_scheduler_request(scheduler_request);
            token.mark_resolved();
            return Ok(ParkCommit::Notified);
        }
        cpu.finish_park_preemption(false);
        cpu.as_mut().clear_current();
        let next = self.pick_owner_next(cpu.as_mut(), now_ns, Some(token.thread()))?;
        if next.outgoing_migration.is_some() {
            return Err(TaskError::InvalidConfiguration);
        }
        let next_core = next.core;
        Self::stage_switch_handoff(
            cpu.as_mut(),
            Some(token.thread()),
            Some(Arc::clone(&previous_core)),
            next_core.id(),
            None,
        )?;
        let decision = Self::owner_switch_plan(
            Some(&previous_core),
            &next_core,
            SwitchReason::Blocked,
            now_ns,
        );
        let decision = self.finish_owner_selection(cpu.as_mut(), decision);
        cpu.acknowledge_scheduler_request(scheduler_request);
        token.mark_resolved();
        Ok(ParkCommit::Blocked(decision))
    }

    /// Cancels a prepared park because an independent grant won the race.
    pub fn cancel_park(
        &self,
        cpu: Pin<&mut CpuLocal>,
        token: &mut ParkTicket,
    ) -> Result<(), TaskError> {
        self.ensure_owner_cpu_context(&cpu)?;
        if token.is_resolved() {
            return Err(TaskError::StaleThreadId);
        }
        self.ensure_owner_cpu_online(&cpu)?;
        if cpu.current() != Some(token.thread()) {
            return Err(TaskError::StaleThreadId);
        }
        let core = cpu.current_core().ok_or(TaskError::NoRunnableThread)?;
        if core.park_generation() != token.generation() {
            return Err(TaskError::StaleThreadId);
        }
        core.sched().lock().transition(core, ThreadState::Running)?;
        cpu.finish_park_preemption(true);
        token.mark_resolved();
        Ok(())
    }

    /// Parks the current thread and selects its replacement.
    pub fn block_current(
        &self,
        mut cpu: Pin<&mut CpuLocal>,
    ) -> Result<ScheduleDecision, TaskError> {
        self.ensure_owner_cpu_context(&cpu)?;
        let now_ns = cpu.update_rq_clock().wall_nanos();
        match self.prepare_park(cpu.as_mut())? {
            ParkPrepare::Prepared(mut ticket) => {
                match self.commit_park_at(cpu.as_mut(), &mut ticket, now_ns)? {
                    ParkCommit::Blocked(decision) => Ok(decision),
                    ParkCommit::Notified => {
                        let core = cpu.current_core().ok_or(TaskError::NoRunnableThread)?;
                        Ok(Self::owner_switch_plan(
                            Some(core),
                            core,
                            SwitchReason::Blocked,
                            now_ns,
                        ))
                    }
                }
            }
            ParkPrepare::Notified => {
                let core = cpu.current_core().ok_or(TaskError::NoRunnableThread)?;
                Ok(Self::owner_switch_plan(
                    Some(core),
                    core,
                    SwitchReason::Blocked,
                    now_ns,
                ))
            }
        }
    }

    /// Validates all fallible current-thread exit prerequisites without
    /// publishing the thread as exited.
    pub(crate) fn prepare_current_exit(
        &self,
        cpu: Pin<&mut CpuLocal>,
    ) -> Result<CurrentExitPermit, TaskError> {
        let now_ns = cpu.update_rq_clock().wall_nanos();
        self.prepare_current_exit_inner(cpu, now_ns, true)
    }

    pub(super) fn prepare_current_exit_inner(
        &self,
        mut cpu: Pin<&mut CpuLocal>,
        now_ns: u64,
        require_runtime_context: bool,
    ) -> Result<CurrentExitPermit, TaskError> {
        self.ensure_owner_cpu_context(&cpu)?;
        self.complete_context_switch(cpu.as_mut())?;
        self.drain_owner_work(cpu.as_mut(), now_ns)?;
        let current = cpu.current().ok_or(TaskError::NoRunnableThread)?;
        if cpu.idle() == Some(current) {
            return Err(TaskError::InvalidConfiguration);
        }
        let current_core = cpu
            .current_core()
            .cloned()
            .ok_or(TaskError::NoRunnableThread)?;
        // Close before taking registry or thread-state locks. An activity that
        // won before this edge may need either lock to finish, just as Linux
        // takes p->pi_lock before rq/task-state validation rather than waiting
        // for a reader while holding rq.
        let scheduler_exit = current_core
            .close_owned_scheduler_activity()
            .ok_or(TaskError::ThreadBusy)?;
        let state = self.state.lock();
        state.ensure_cpu_online(&cpu)?;
        let record = state.thread_record(current)?;
        if !Arc::ptr_eq(&record.core, &current_core) {
            return Err(TaskError::StaleThreadId);
        }
        let sched = record.sched.lock();
        let lifecycle = sched.lifecycle.state();
        if lifecycle != ThreadState::Running {
            return Err(TaskError::InvalidTransition {
                from: lifecycle,
                to: ThreadState::Exited,
            });
        }
        if record.blocked_on.is_some()
            || !record.pi_donors.is_empty()
            || sched.pi.donating_locks != 0
        {
            return Err(TaskError::InvalidPiState);
        }
        if sched.placement.execution_cpu() != Some(cpu.owner())
            || sched.placement.on_cpu() != Some(cpu.owner())
        {
            return Err(TaskError::ThreadBusy);
        }
        if require_runtime_context && record.resources.context().is_none() {
            return Err(TaskError::InvalidRuntimeHandle);
        }
        record.callbacks.validate_prepare_exit()?;
        Ok(CurrentExitPermit {
            thread: current,
            scheduler_exit,
        })
    }

    /// Atomically prepares and commits current-thread exit.
    ///
    /// Runtime integrations that publish OS completion between those phases
    /// use the crate-private prepared form instead.
    pub fn exit_current(&self, mut cpu: Pin<&mut CpuLocal>) -> Result<ScheduleDecision, TaskError> {
        // Pure scheduler users may model a transition without installing an
        // architecture context. The runtime facade uses the stricter prepared
        // form before publishing OS-visible completion.
        let now_ns = cpu.update_rq_clock().wall_nanos();
        let permit = self.prepare_current_exit_inner(cpu.as_mut(), now_ns, false)?;
        self.commit_current_exit_after_owner_drain(cpu, permit, now_ns)
    }

    /// Commits a prepared current-thread exit and selects a replacement.
    pub(crate) fn commit_prepared_current_exit(
        &self,
        mut cpu: Pin<&mut CpuLocal>,
        permit: CurrentExitPermit,
    ) -> Result<ScheduleDecision, TaskError> {
        self.ensure_owner_cpu_context(&cpu)?;
        let now_ns = cpu.update_rq_clock().wall_nanos();
        self.complete_context_switch(cpu.as_mut())?;
        self.drain_owner_work(cpu.as_mut(), now_ns)?;
        self.commit_current_exit_after_owner_drain(cpu, permit, now_ns)
    }

    /// Commits the non-returning half of current exit after owner work drained.
    ///
    /// The move-only permit has already closed new scheduler activity. A
    /// message whose delivery reservation predates that close remains an
    /// in-flight late delivery and pins registry resources until its owner
    /// drains it as an exited no-op.
    pub(super) fn commit_current_exit_after_owner_drain(
        &self,
        mut cpu: Pin<&mut CpuLocal>,
        mut permit: CurrentExitPermit,
        now_ns: u64,
    ) -> Result<ScheduleDecision, TaskError> {
        let exiting = permit.thread();
        let (decision, exited_core, scheduler_request) = {
            let mut state = self.state.lock();
            state.ensure_cpu_online(&cpu)?;
            let previous = cpu.current().ok_or(TaskError::NoRunnableThread)?;
            if previous != exiting {
                return Err(TaskError::StaleThreadId);
            }
            let previous_core = cpu.current_core().cloned();
            let record = state.thread_record(previous)?;
            if record.has_live_pi_edges() {
                return Err(TaskError::InvalidPiState);
            }
            record.callbacks.validate_prepare_exit()?;
            let scheduler_request = cpu.as_mut().claim_scheduler_request();
            self.commit_owner_current_dispatch(cpu.as_mut())?;
            let previous_core = previous_core.ok_or(TaskError::NoRunnableThread)?;
            Self::detach_owner_deadline_bandwidth(&previous_core, cpu.as_mut());
            let held_reservation;
            {
                let mut sched = previous_core.sched().lock();
                if sched.lifecycle.state() != ThreadState::Running
                    || sched.placement.execution_cpu() != Some(cpu.owner())
                    || sched.placement.on_cpu() != Some(cpu.owner())
                {
                    return Err(TaskError::InvalidConfiguration);
                }
                let mut run_queue = cpu.lock_run_queue();
                if run_queue.is_linked_current(previous) {
                    run_queue
                        .dequeue(previous)
                        .expect("validated retained current must remain linked");
                }
                if sched
                    .transition(&previous_core, ThreadState::Exited)
                    .is_err()
                {
                    task_runtime::fatal_invariant(0x4558_0001, previous.as_u64() as usize);
                }
                sched.placement.block_current(cpu.owner());
                // From this point a recoverable return is impossible. Keep the
                // gate permanently closed so reaping may treat a zero delivery
                // count as stable after observing Exited.
                permit.seal();
                held_reservation = sched
                    .deadline
                    .active_reservation
                    .max(sched.deadline.desired_reservation);
                sched.deadline.active_reservation = 0;
                sched.deadline.desired_reservation = 0;
                let record = state.thread_record_mut(previous).unwrap_or_else(|_| {
                    task_runtime::fatal_invariant(0x4558_0002, previous.as_u64() as usize)
                });
                if record
                    .callbacks
                    .prepare_exit(record.extension.is_some())
                    .is_err()
                {
                    task_runtime::fatal_invariant(0x4558_0003, previous.as_u64() as usize);
                }
            }
            state.queue_exited_thread(previous);
            let mut root_domain = self.root_domain.lock();
            root_domain.release_deadline(held_reservation);
            cpu.as_mut().clear_current();
            let next = self
                .pick_owner_next(cpu.as_mut(), now_ns, Some(previous))
                .unwrap_or_else(|_| {
                    task_runtime::fatal_invariant(0x4558_0004, previous.as_u64() as usize)
                });
            if next.outgoing_migration.is_some() {
                task_runtime::fatal_invariant(0x4558_0005, previous.as_u64() as usize);
            }
            let next_core = next.core;
            if Self::stage_switch_handoff(
                cpu.as_mut(),
                Some(previous),
                Some(Arc::clone(&previous_core)),
                next_core.id(),
                None,
            )
            .is_err()
            {
                task_runtime::fatal_invariant(0x4558_0006, previous.as_u64() as usize);
            }
            (
                Self::owner_switch_plan(
                    Some(&previous_core),
                    &next_core,
                    SwitchReason::Exited,
                    now_ns,
                ),
                Arc::clone(&previous_core),
                scheduler_request,
            )
        };
        exited_core.notify_affinity_waiters();
        drop(permit);
        let decision = self.finish_owner_selection(cpu.as_mut(), decision);
        cpu.acknowledge_scheduler_request(scheduler_request);
        Ok(decision)
    }

    /// Completes the physical switch-out handoff in the newly active context.
    ///
    /// This second phase clears `on_cpu` only after architecture execution has
    /// left the previous stack. Deferred migration publication and exit hooks
    /// therefore cannot make a context runnable or reapable too early.
    #[doc(hidden)]
    pub fn complete_context_switch(
        &self,
        mut cpu: Pin<&mut CpuLocal>,
    ) -> Result<SwitchInCompletion, TaskError> {
        self.ensure_owner_cpu_context(&cpu)?;
        let Some(initial_handoff) = cpu.as_ref().get_ref().switch_handoff() else {
            return Ok(SwitchInCompletion::NONE);
        };
        let owner = cpu.owner();
        let previous_core = Arc::clone(&initial_handoff.previous);
        let migration_target = initial_handoff.migration_target();
        let runtime_tail_finished = initial_handoff.runtime_tail_finished;
        {
            let bandwidth = cpu.as_ref().get_ref().deadline_bandwidth();
            let sched = previous_core.sched().lock();
            self.validate_switch_handoff_state(owner, bandwidth, initial_handoff, &sched)?;
        }

        if !runtime_tail_finished {
            task_runtime::finish_context_switch_tail();
            if cpu
                .as_mut()
                .finish_switch_runtime_tail(previous_core.id(), migration_target)
                .is_err()
            {
                task_runtime::fatal_invariant(0x5357_0001, previous_core.id().as_u64() as usize);
            }
        }

        let handoff = cpu
            .as_ref()
            .get_ref()
            .switch_handoff()
            .ok_or(TaskError::InvalidConfiguration)?;
        let previous = handoff.previous.id();
        let incoming = cpu
            .current_core()
            .cloned()
            .ok_or(TaskError::NoRunnableThread)?;
        if incoming.id() == previous {
            return Err(TaskError::InvalidConfiguration);
        }
        let (migration_target, previous_exited, wake_after_tail, affinity_completed) = {
            let bandwidth = cpu.as_ref().get_ref().deadline_bandwidth();
            let mut sched = handoff.previous.sched().lock();
            let (migration_target, previous_exited) =
                self.validate_switch_handoff_state(owner, bandwidth, handoff, &sched)?;
            if migration_target.is_some() && sched.deadline.bandwidth_cpu.is_some() {
                let mut run_queue = cpu.lock_run_queue();
                run_queue.remove_deadline_bandwidth(
                    sched.deadline.bandwidth_scaled,
                    sched.deadline.activity != DeadlineActivity::Inactive,
                );
                sched.deadline.bandwidth_cpu = None;
                run_queue.unregister_deadline_member(&previous_core);
            }
            sched.placement.finish_task(owner);
            if let Some(target) = migration_target {
                previous_core.set_wake_cpu_hint(target);
            }
            let affinity_completed =
                Self::complete_affinity_if_satisfied_locked(&previous_core, &sched);
            (
                migration_target,
                previous_exited,
                sched.lifecycle.state() == ThreadState::Waking,
                affinity_completed,
            )
        };
        if affinity_completed {
            previous_core.notify_affinity_waiters();
        }
        let mut consumed = cpu.as_mut().take_switch_handoff().unwrap_or_else(|| {
            task_runtime::fatal_invariant(0x5357_0003, previous.as_u64() as usize)
        });
        if consumed.previous.id() != previous
            || consumed.migration_target() != migration_target
            || !consumed.runtime_tail_finished
        {
            task_runtime::fatal_invariant(0x5357_0004, previous.as_u64() as usize);
        }
        if let Some(migration) = consumed.migration.take() {
            migration.commit();
        }
        if wake_after_tail {
            self.finish_switch_tail_wake(&previous_core);
        }
        self.publish_owner_cpu_load_summary(cpu.as_mut());
        if previous_exited {
            self.task_work.publish();
        }
        Ok(SwitchInCompletion::for_core(&incoming))
    }

    fn validate_switch_handoff_state(
        &self,
        owner: CpuId,
        bandwidth: DeadlineBandwidthSnapshot,
        handoff: &crate::system::cpu::SwitchHandoff,
        sched: &ThreadSchedState,
    ) -> Result<(Option<CpuId>, bool), TaskError> {
        if sched.placement.on_cpu() != Some(owner) {
            return Err(TaskError::InvalidConfiguration);
        }
        let migration_target = match handoff.migration_target() {
            Some(reserved_target) => {
                let target = sched
                    .placement
                    .committed_migration_target()
                    .ok_or(TaskError::InvalidConfiguration)?;
                if target != reserved_target {
                    return Err(TaskError::InvalidConfiguration);
                }
                if sched.lifecycle.state() != ThreadState::Ready
                    || sched.placement.queued_cpu().is_some()
                    || sched.placement.execution_cpu().is_some()
                {
                    return Err(TaskError::InvalidConfiguration);
                }
                if let Some(assigned) = sched.deadline.bandwidth_cpu {
                    if assigned != owner {
                        return Err(TaskError::CpuOwnerMismatch {
                            expected: assigned.as_u32(),
                            actual: owner.as_u32(),
                        });
                    }
                    if bandwidth.this_bw_scaled() < sched.deadline.bandwidth_scaled
                        || (sched.deadline.activity != DeadlineActivity::Inactive
                            && bandwidth.running_bw_scaled() < sched.deadline.bandwidth_scaled)
                    {
                        return Err(TaskError::InvalidConfiguration);
                    }
                }
                Some(target)
            }
            None => None,
        };
        Ok((
            migration_target,
            sched.lifecycle.state() == ThreadState::Exited,
        ))
    }
}
