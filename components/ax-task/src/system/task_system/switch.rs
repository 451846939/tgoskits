//! Owner selection, schedule-out, and switch-handoff construction.

use super::*;
use crate::scheduler::RtEligibility;

impl TaskSystem {
    pub(super) fn capture_owner_fair_migration(
        &self,
        cpu: &CpuLocal,
        sched: &mut ThreadSchedState,
    ) {
        let timing_granularity_ns = self.config.timing_granularity_ns();
        let run_queue = cpu.lock_run_queue();
        if let Some(fair) = sched.policy.effective_entity.fair() {
            let virtual_time = run_queue.virtual_time_for_mode(fair.mode());
            sched
                .policy
                .effective_entity
                .capture_fair_migration(virtual_time, timing_granularity_ns);
        }
        if !sched.is_pi_boosted() {
            sched.policy.base_entity = sched.policy.effective_entity;
        } else if let Some(fair) = sched.policy.base_entity.fair() {
            let virtual_time = run_queue.virtual_time_for_mode(fair.mode());
            sched
                .policy
                .base_entity
                .capture_fair_migration(virtual_time, timing_granularity_ns);
        }
    }

    /// Completes every owner-side selection through the same balance and
    /// one-shot programming sequence.
    ///
    /// Forced block and exit paths select a successor just like preemption and
    /// yield. Keeping their tail common prevents a tickless CPU from retaining
    /// the outgoing thread's budget or service deadline after the switch plan
    /// has already committed a different scheduling class.
    pub(super) fn finish_owner_selection(
        &self,
        mut cpu: Pin<&mut CpuLocal>,
        decision: ScheduleDecision,
    ) -> ScheduleDecision {
        // Selection, lifecycle, and switch-handoff state are already committed
        // before this tail. Reporting a recoverable error would let block or
        // yield callers attempt to resume an outgoing thread that is no longer
        // current, so runtime failures beyond this boundary are fatal.
        if self.owner_balance_work_pending(cpu.as_ref().get_ref(), decision.next())
            && self
                .service_owner_balance(cpu.as_mut(), decision.next())
                .is_err()
        {
            task_runtime::fatal_invariant(0x5343_0001, decision.next().as_u64() as usize);
        }
        if Self::program_local_timer(cpu.as_mut()).is_err() {
            task_runtime::fatal_invariant(0x5343_0002, decision.next().as_u64() as usize);
        }
        decision
    }

    /// Commits one running owner either to its local queue, a migration
    /// handoff, or Deadline throttle state.
    ///
    /// Remote affinity writers use the same stable thread cell. Keeping the
    /// affinity decision, lifecycle transition, and local enqueue under this
    /// one guard is the scheduler equivalent of Linux's task/rq locking rule:
    /// an affinity update cannot invalidate a placement snapshot between
    /// observing it and clearing `CpuLocal::current`.
    pub(super) fn schedule_out_owner_running(
        &self,
        mut cpu: Pin<&mut CpuLocal>,
        core: Arc<ThreadCore>,
        now_ns: u64,
        reason: EnqueueReason,
    ) -> Result<Option<PreparedMigrationDelivery>, TaskError> {
        self.ensure_owner_cpu_online(&cpu)?;
        let owner = cpu.owner();
        let mut sched = core.sched().lock();
        let retained_current = cpu.lock_run_queue().is_linked_current(core.id());
        if sched.lifecycle.state() != ThreadState::Running
            || sched.placement.execution_cpu() != Some(owner)
            || sched.placement.on_cpu() != Some(owner)
        {
            return Err(TaskError::InvalidConfiguration);
        }

        let migration_requested = sched.placement.requested_migration().is_some()
            || !sched.placement.affinity.contains(owner);
        if migration_requested {
            let target = sched
                .placement
                .requested_migration()
                .filter(|target| {
                    *target != owner
                        && sched.placement.affinity.contains(*target)
                        && self
                            .cpu_remotes
                            .get(target.as_usize())
                            .is_some_and(|remote| remote.accepts_placement())
                })
                .or_else(|| self.select_allowed_active_cpu(&sched.placement.affinity, Some(owner)))
                .ok_or(TaskError::InvalidConfiguration)?;
            let migration = self.prepare_owner_migration(&core, owner, target)?;
            self.capture_owner_fair_migration(cpu.as_ref().get_ref(), &mut sched);
            sched.transition(&core, ThreadState::Ready)?;
            if retained_current {
                let mut run_queue = cpu.lock_run_queue();
                run_queue
                    .dequeue(core.id())
                    .expect("validated retained current must remain linked");
                sched.placement.begin_migration(owner, target);
            } else {
                sched.placement.begin_migration(owner, target);
            }
            core.set_wake_cpu_hint(target);
            cpu.as_mut().clear_current();
            return Ok(Some(migration));
        }

        if sched.policy.effective_entity.is_deadline_throttled() && !sched.pi.critical_rescue {
            if let SchedulingEntity::Deadline(_) = sched.policy.effective_entity {
                if !sched.is_pi_boosted() {
                    sched.policy.base_entity = sched.policy.effective_entity;
                }
                sched.deadline.replenish_pending = true;
            }
            sched.transition(&core, ThreadState::Blocked)?;
            if retained_current {
                let mut run_queue = cpu.lock_run_queue();
                run_queue
                    .dequeue(core.id())
                    .expect("validated retained current must remain linked");
                sched.placement.block_current(owner);
            } else {
                sched.placement.block_current(owner);
            }
            cpu.as_mut().clear_current();
            self.refresh_owner_deadline_timers_locked(&core, &mut sched, cpu.as_mut(), now_ns)?;
            return Ok(None);
        }

        if cpu.idle() == Some(core.id()) {
            sched.transition(&core, ThreadState::Ready)?;
            // Linux idle is a per-CPU runnable task: put_prev_task_idle()
            // updates accounting but never dequeues it from rq ownership.
            sched.placement.put_prev(owner);
            cpu.as_mut().clear_current();
            return Ok(None);
        }

        if retained_current {
            // Timer replacement is the only recoverable preparation in the
            // retained RT/DL path. Complete it before mutating runqueue or
            // placement ownership, like Linux prepares class state before
            // the rq-locked put-prev/set-next commit.
            self.refresh_owner_deadline_timers_locked(&core, &mut sched, cpu.as_mut(), now_ns)?;
        }

        // Hide the outgoing dispatch while queue placement computes EEVDF
        // virtual time, but retain it until enqueue commits. A typed enqueue
        // failure can therefore restore the Running owner without publishing
        // a transient `current = None` state.
        let dispatch = cpu.as_mut().take_dispatch();
        if let Err(error) = sched.transition(&core, ThreadState::Ready) {
            if let Some(dispatch) = dispatch {
                cpu.as_mut().install_dispatch(dispatch);
            }
            return Err(error);
        }
        let preempts_current = if retained_current {
            let mut run_queue = cpu.lock_run_queue();
            let queued_entity = run_queue
                .put_prev_linked(
                    QueuedThread::new(
                        core.id(),
                        sched.policy.effective,
                        sched.policy.effective_entity,
                        Arc::clone(&core),
                        sched.is_pi_boosted_rt_owner(),
                        sched.placement.affinity.is_migration_capable(),
                    ),
                    reason,
                )
                .unwrap_or_else(|_| {
                    task_runtime::fatal_invariant(0x5343_0005, core.id().as_u64() as usize)
                });
            sched.placement.put_prev(owner);
            sched.policy.effective_entity = queued_entity;
            if !sched.is_pi_boosted() {
                sched.policy.base_entity = queued_entity;
            }
            core.publish_effective_schedule(sched.policy.effective, queued_entity);
            core.set_wake_cpu_hint(owner);
            false
        } else {
            match self.enqueue_owner_thread_locked(cpu.as_mut(), &core, &mut sched, reason) {
                Ok(preempts_current) => preempts_current,
                Err(error) => {
                    let rollback = sched.transition(&core, ThreadState::Running);
                    if let Some(dispatch) = dispatch {
                        cpu.as_mut().install_dispatch(dispatch);
                    }
                    rollback?;
                    return Err(error);
                }
            }
        };
        cpu.as_mut().clear_current();
        drop(sched);
        drop(dispatch);
        self.finish_owner_enqueue(cpu, reason, preempts_current);
        Ok(None)
    }

    pub(super) fn select_allowed_active_cpu(
        &self,
        affinity: &CpuSet,
        excluded: Option<CpuId>,
    ) -> Option<CpuId> {
        self.cpu_remotes
            .iter()
            .enumerate()
            .filter_map(|(index, remote)| {
                let cpu = CpuId::new(index as u32);
                (Some(cpu) != excluded && remote.accepts_placement() && affinity.contains(cpu))
                    .then_some(cpu)
                    .and_then(|cpu| {
                        remote
                            .try_runnable_summary()
                            .map(|runnable| (runnable, cpu))
                    })
            })
            .min_by_key(|(load, cpu)| (*load, cpu.as_u32()))
            .map(|(_, cpu)| cpu)
    }

    pub(super) fn select_priority_cpu(
        &self,
        policy: SchedulePolicy,
        entity: SchedulingEntity,
        affinity: &CpuSet,
        preferred: Option<CpuId>,
        excluded: Option<CpuId>,
    ) -> Option<CpuId> {
        let accepts = |cpu: CpuId| {
            Some(cpu) != excluded
                && self
                    .cpu_remotes
                    .get(cpu.as_usize())
                    .is_some_and(|remote| remote.accepts_placement())
        };
        let indexed = match policy {
            SchedulePolicy::KernelStop => None,
            SchedulePolicy::Fifo { priority } | SchedulePolicy::RoundRobin { priority, .. } => self
                .root_domain
                .find_lowest_rt_cpu(priority, affinity, preferred, accepts),
            SchedulePolicy::Deadline(_) => entity
                .deadline()
                .and_then(DeadlineEntity::absolute_deadline_ns)
                .and_then(|absolute_deadline_ns| {
                    self.root_domain.find_later_deadline_cpu(
                        absolute_deadline_ns,
                        affinity,
                        preferred,
                        accepts,
                    )
                }),
            SchedulePolicy::Fair { .. } => None,
        };
        indexed
            .or_else(|| preferred.filter(|cpu| affinity.contains(*cpu) && accepts(*cpu)))
            .or_else(|| self.select_allowed_active_cpu(affinity, excluded))
    }

    fn validate_owner_next(
        sched: &ThreadSchedState,
        next: ThreadId,
        owner: CpuId,
        outgoing: Option<ThreadId>,
    ) -> Result<(), TaskError> {
        match sched.placement.on_cpu() {
            None => Ok(()),
            Some(executing_cpu) if outgoing == Some(next) && executing_cpu == owner => Ok(()),
            Some(_) => Err(TaskError::InvalidConfiguration),
        }
    }

    pub(super) fn pick_owner_next(
        &self,
        mut cpu: Pin<&mut CpuLocal>,
        now_ns: u64,
        outgoing: Option<ThreadId>,
    ) -> Result<OwnerNext, TaskError> {
        let owner = cpu.owner();
        let mut outgoing_migration = None;
        let mut reconciled = 0;
        let core = loop {
            let queued = {
                let dispatch = cpu.as_mut().dispatch_state_mut();
                let rt_eligibility = if dispatch.rt_bandwidth.may_run(now_ns, false) {
                    RtEligibility::Ordinary
                } else {
                    RtEligibility::PiOwnerOnly
                };
                cpu.lock_run_queue().pick_next(rt_eligibility)
            };
            let Some(queued) = queued else {
                break cpu
                    .as_ref()
                    .get_ref()
                    .dispatch_state()
                    .idle_core
                    .as_ref()
                    .cloned()
                    .ok_or(TaskError::NoRunnableThread)?;
            };
            let core = Arc::clone(&queued.core);
            let mut sched = core.sched().lock();
            Self::validate_owner_next(&sched, core.id(), owner, outgoing)?;
            let migration_target = if sched.placement.requested_migration().is_some()
                || !sched.placement.affinity.contains(owner)
            {
                Some(
                    sched
                        .placement
                        .requested_migration()
                        .filter(|target| {
                            *target != owner
                                && sched.placement.affinity.contains(*target)
                                && self
                                    .cpu_remotes
                                    .get(target.as_usize())
                                    .is_some_and(|remote| remote.accepts_placement())
                        })
                        .or_else(|| {
                            self.select_allowed_active_cpu(&sched.placement.affinity, Some(owner))
                        })
                        .ok_or(TaskError::InvalidConfiguration)?,
                )
            } else {
                None
            };
            sched.policy.effective_entity = queued.entity;
            if !sched.is_pi_boosted() {
                sched.policy.base_entity = queued.entity;
            }
            if let Some(target) = migration_target {
                let outgoing_candidate =
                    outgoing == Some(core.id()) && sched.placement.on_cpu() == Some(owner);
                let carrier = match self.prepare_owner_migration(&core, owner, target) {
                    Ok(carrier) => carrier,
                    Err(error) => {
                        drop(sched);
                        cpu.lock_run_queue().rollback_pick(queued);
                        return Err(error);
                    }
                };
                if !outgoing_candidate {
                    Self::detach_owner_deadline_bandwidth_locked(&core, &mut sched, cpu.as_mut());
                }
                self.capture_owner_fair_migration(cpu.as_ref().get_ref(), &mut sched);
                sched.placement.begin_migration(owner, target);
                if cpu.lock_run_queue().is_linked_current(core.id()) {
                    cpu.lock_run_queue()
                        .dequeue(core.id())
                        .expect("selected RT/DL migration must remain linked");
                }
                core.set_wake_cpu_hint(target);
                drop(sched);
                if outgoing_candidate {
                    outgoing_migration = Some(carrier);
                } else {
                    carrier.commit();
                }
                reconciled += 1;
                if reconciled == cpu.batch_limit() {
                    cpu.request_scheduler_work();
                    break cpu
                        .as_ref()
                        .get_ref()
                        .dispatch_state()
                        .idle_core
                        .as_ref()
                        .cloned()
                        .ok_or(TaskError::NoRunnableThread)?;
                }
                continue;
            }
            if sched.lifecycle.state() != ThreadState::Ready
                || sched.placement.queued_cpu() != Some(owner)
            {
                drop(sched);
                cpu.lock_run_queue().rollback_pick(queued);
                return Err(TaskError::InvalidConfiguration);
            }
            let dispatch = match Self::owner_dispatch(&core, &sched, now_ns) {
                Ok(dispatch) => dispatch,
                Err(error) => {
                    drop(sched);
                    cpu.lock_run_queue().rollback_pick(queued);
                    return Err(error);
                }
            };
            sched.placement.set_next_task(owner);
            if sched.transition(&core, ThreadState::Running).is_err() {
                task_runtime::fatal_invariant(0x5349_0001, core.id().as_u64() as usize);
            }
            drop(sched);
            cpu.as_mut().install_dispatch(dispatch);
            break core;
        };
        if cpu.as_ref().get_ref().idle() == Some(core.id()) {
            let mut sched = core.sched().lock();
            Self::validate_owner_next(&sched, core.id(), owner, outgoing)?;
            if sched.lifecycle.state() == ThreadState::Ready {
                sched.transition(&core, ThreadState::Running)?;
            }
            if sched.placement.queued_cpu().is_none() {
                sched.placement.activate(owner);
            }
            if sched.placement.on_cpu().is_none() {
                sched.placement.set_next_task(owner);
            }
            let dispatch = Self::owner_dispatch(&core, &sched, now_ns)?;
            cpu.as_mut().install_dispatch(dispatch);
        }
        cpu.as_mut().set_current_core(Arc::clone(&core));
        self.publish_owner_cpu_load_summary(cpu.as_mut());
        Ok(OwnerNext {
            core,
            outgoing_migration,
        })
    }

    pub(super) fn stage_switch_handoff(
        mut cpu: Pin<&mut CpuLocal>,
        previous: Option<ThreadId>,
        previous_core: Option<Arc<ThreadCore>>,
        next: ThreadId,
        migration: Option<PreparedMigrationDelivery>,
    ) -> Result<(), TaskError> {
        match previous {
            Some(previous) if previous != next => {
                let previous_core = previous_core.ok_or(TaskError::InvalidConfiguration)?;
                if previous_core.id() != previous {
                    return Err(TaskError::InvalidConfiguration);
                }
                cpu.as_mut().stage_switch_handoff(previous_core, migration)
            }
            _ if migration.is_none() => Ok(()),
            _ => Err(TaskError::InvalidConfiguration),
        }
    }

    pub(super) fn owner_switch_plan(
        previous: Option<&Arc<ThreadCore>>,
        next: &Arc<ThreadCore>,
        switch_reason: SwitchReason,
        timestamp_ns: u64,
    ) -> ScheduleDecision {
        ScheduleDecision {
            previous: previous.map(|core| core.id()),
            next: next.id(),
            previous_endpoint: previous.map(|core| SwitchEndpoint::from_core(core)),
            next_endpoint: SwitchEndpoint::from_core(next),
            switch_reason,
            timestamp_ns,
        }
    }
}
