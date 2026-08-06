//! Owner-local dispatch accounting and switch-tail handoff.

use super::*;
use crate::{DeadlineEntity, scheduler_time_advance};

/// State committed before an architecture switch and consumed by switch tail.
#[derive(Debug)]
pub(crate) struct SwitchHandoff {
    pub(crate) previous: Arc<ThreadCore>,
    pub(crate) migration: Option<PreparedMigrationDelivery>,
    /// The architecture tail has irrevocably left `previous`'s context.
    pub(crate) runtime_tail_finished: bool,
}

impl SwitchHandoff {
    pub(crate) fn migration_target(&self) -> Option<CpuId> {
        self.migration
            .as_ref()
            .map(PreparedMigrationDelivery::target)
    }
}
/// Owner-CPU copy of the running thread's mutable dispatch accounting.
///
/// Timer IRQ mutates only this object. The scheduler commits it to the registry
/// at the next safe point, so hard IRQ never acquires the global task-system lock.
#[derive(Debug)]
pub(crate) struct CurrentDispatch {
    pub(crate) thread: ThreadId,
    pub(crate) policy: SchedulePolicy,
    pub(crate) entity: SchedulingEntity,
    pub(crate) deadline_donor: Option<ThreadId>,
    pub(crate) blocks_pi_waiter: bool,
    pub(crate) rt_quota_exempt: bool,
    pub(crate) pi_critical_rescue: bool,
    pub(crate) policy_generation: u64,
    pub(crate) deadline_overrun: bool,
    runtime_core: Arc<ThreadCore>,
    deadline_donor_core: Option<Arc<ThreadCore>>,
    deadline_cbs_generation: Option<u64>,
    accounted_until_ns: u64,
    charged_runtime_ns: u64,
}

/// Registry state copied into one owner-CPU dispatch interval.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CurrentDispatchState {
    pub(crate) thread: ThreadId,
    pub(crate) policy: SchedulePolicy,
    pub(crate) entity: SchedulingEntity,
    pub(crate) deadline_donor: Option<ThreadId>,
    pub(crate) blocks_pi_waiter: bool,
    pub(crate) rt_quota_exempt: bool,
    pub(crate) pi_critical_rescue: bool,
    pub(crate) policy_generation: u64,
}

/// Copy-only current scheduling state observed under the runqueue lock.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DispatchRole {
    Task,
    DedicatedIdle,
}

/// Copy-only current scheduling state observed under the runqueue lock.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CurrentSchedule {
    thread: ThreadId,
    policy: SchedulePolicy,
    entity: SchedulingEntity,
    role: DispatchRole,
}

impl CurrentSchedule {
    #[cfg(test)]
    pub(crate) const fn test_state(
        thread: ThreadId,
        policy: SchedulePolicy,
        entity: SchedulingEntity,
    ) -> Self {
        Self {
            thread,
            policy,
            entity,
            role: DispatchRole::Task,
        }
    }

    pub(crate) const fn thread(self) -> ThreadId {
        self.thread
    }

    pub(crate) const fn schedule_policy(self) -> SchedulePolicy {
        self.policy
    }

    pub(crate) const fn scheduling_entity(self) -> SchedulingEntity {
        self.entity
    }

    pub(crate) const fn absolute_deadline_ns(self) -> Option<u64> {
        if matches!(self.role, DispatchRole::DedicatedIdle) {
            return None;
        }
        match self.entity.deadline() {
            Some(deadline) => deadline.absolute_deadline_ns(),
            None => None,
        }
    }

    pub(crate) const fn fair_entity(self) -> Option<crate::FairEntity> {
        if matches!(self.role, DispatchRole::DedicatedIdle) {
            None
        } else {
            self.entity.fair()
        }
    }

    pub(crate) const fn scheduling_key(self) -> SchedulingKey {
        match self.entity {
            SchedulingEntity::Fair(fair) => SchedulingKey::new(
                self.policy.class_rank(),
                fair.virtual_deadline(),
                self.thread.as_u64(),
            ),
            _ => self
                .entity
                .scheduling_key(self.policy, self.thread.as_u64()),
        }
    }

    pub(crate) const fn placement_demand(self) -> u64 {
        if matches!(self.role, DispatchRole::DedicatedIdle) {
            0
        } else {
            self.policy.placement_demand()
        }
    }

    pub(crate) const fn fair_demand(self) -> u64 {
        if matches!(self.role, DispatchRole::DedicatedIdle) {
            0
        } else {
            self.policy.fair_demand()
        }
    }

    pub(crate) fn should_preempt(
        self,
        woken_policy: SchedulePolicy,
        woken_entity: SchedulingEntity,
        fair_virtual_time: u64,
    ) -> bool {
        // Linux's per-CPU idle task belongs to idle_sched_class, not to the
        // fair SCHED_IDLE policy. Every runnable class therefore preempts it,
        // including ordinary FairMode::Idle work.
        if matches!(self.role, DispatchRole::DedicatedIdle) {
            return true;
        }
        match woken_policy {
            SchedulePolicy::KernelStop => !matches!(self.policy, SchedulePolicy::KernelStop),
            SchedulePolicy::Deadline(_) => match self.policy {
                SchedulePolicy::KernelStop => false,
                SchedulePolicy::Deadline(_) => {
                    deadline_key(woken_entity) < deadline_key(self.entity)
                }
                _ => true,
            },
            SchedulePolicy::Fifo { priority } | SchedulePolicy::RoundRobin { priority, .. } => {
                match self.policy {
                    SchedulePolicy::KernelStop | SchedulePolicy::Deadline(_) => false,
                    SchedulePolicy::Fifo { priority: current }
                    | SchedulePolicy::RoundRobin {
                        priority: current, ..
                    } => priority > current,
                    SchedulePolicy::Fair { .. } => true,
                }
            }
            SchedulePolicy::Fair {
                mode: woken_mode, ..
            } => match self.policy {
                SchedulePolicy::KernelStop
                | SchedulePolicy::Deadline(_)
                | SchedulePolicy::Fifo { .. }
                | SchedulePolicy::RoundRobin { .. } => false,
                SchedulePolicy::Fair {
                    mode: current_mode, ..
                } => {
                    if woken_mode == FairMode::Idle && current_mode != FairMode::Idle {
                        false
                    } else if woken_mode != FairMode::Idle && current_mode == FairMode::Idle {
                        true
                    } else if woken_mode == FairMode::Batch
                        || woken_entity
                            .fair()
                            .is_none_or(|fair| !fair.is_eligible(fair_virtual_time))
                    {
                        false
                    } else {
                        let woken = woken_entity
                            .fair()
                            .expect("fair policy must own a fair scheduling entity");
                        let current = self
                            .entity
                            .fair()
                            .expect("fair policy must own a fair scheduling entity");
                        (!current.is_eligible(fair_virtual_time) || current.request_exhausted())
                            && woken.deadline_precedes(current)
                    }
                }
            },
        }
    }
}

impl CurrentDispatch {
    pub(crate) fn new(
        state: CurrentDispatchState,
        runtime_core: &Arc<ThreadCore>,
        now_ns: u64,
    ) -> Self {
        runtime_core.begin_runtime_accounting(now_ns);
        Self {
            thread: state.thread,
            policy: state.policy,
            entity: state.entity,
            deadline_donor: state.deadline_donor,
            blocks_pi_waiter: state.blocks_pi_waiter,
            rt_quota_exempt: state.rt_quota_exempt,
            pi_critical_rescue: state.pi_critical_rescue,
            policy_generation: state.policy_generation,
            deadline_overrun: false,
            runtime_core: Arc::clone(runtime_core),
            deadline_donor_core: None,
            deadline_cbs_generation: None,
            accounted_until_ns: now_ns,
            charged_runtime_ns: 0,
        }
    }

    pub(crate) fn with_deadline_donor_core(
        mut self,
        donor: Option<Arc<ThreadCore>>,
        cbs_generation: Option<u64>,
    ) -> Self {
        debug_assert_eq!(self.deadline_donor.is_some(), donor.is_some());
        debug_assert!(cbs_generation.is_none() || donor.is_some());
        self.deadline_donor_core = donor;
        self.deadline_cbs_generation = cbs_generation;
        self
    }

    pub(crate) fn deadline_donor_core(&self) -> Option<&Arc<ThreadCore>> {
        self.deadline_donor_core.as_ref()
    }

    pub(crate) const fn deadline_cbs_generation(&self) -> Option<u64> {
        self.deadline_cbs_generation
    }

    pub(super) fn charge(
        &mut self,
        runtime_ns: u64,
        now_ns: u64,
        reclaimed_ns: u64,
    ) -> DispatchCharge {
        self.charged_runtime_ns = self.charged_runtime_ns.saturating_add(runtime_ns);
        self.accounted_until_ns = now_ns;
        self.runtime_core().charge_runtime(runtime_ns, now_ns);
        if self.pi_critical_rescue {
            return DispatchCharge::default();
        }
        let mut slice_expired = self.entity.charge(runtime_ns, 0, reclaimed_ns);
        let mut deadline_overrun = false;
        if slice_expired && let SchedulePolicy::Deadline(policy) = self.policy {
            deadline_overrun = policy.flags().contains(crate::DeadlineFlags::DL_OVERRUN);
            self.deadline_overrun |= deadline_overrun;
            if self.blocks_pi_waiter {
                self.pi_critical_rescue = true;
                self.entity.enter_pi_critical_rescue();
                slice_expired = false;
            }
        }
        DispatchCharge {
            slice_expired,
            deadline_overrun,
        }
    }

    /// Advances the owner clock for the per-CPU idle dispatch.
    ///
    /// The dedicated idle task is not a Fair entity in Linux's scheduler
    /// model. It must not consume a Fair request or task runtime merely because
    /// the runtime represents its context with a regular thread object.
    pub(super) fn account_dedicated_idle_until(&mut self, now_ns: u64) {
        self.accounted_until_ns = now_ns;
    }

    pub(crate) fn finish_runtime_accounting(&self, now_ns: u64) {
        self.runtime_core().finish_runtime_accounting(now_ns);
    }

    pub(crate) fn take_charged_runtime_ns(&mut self) -> u64 {
        core::mem::take(&mut self.charged_runtime_ns)
    }

    pub(super) fn unaccounted_runtime(&self, now_ns: u64) -> u64 {
        now_ns.saturating_sub(self.accounted_until_ns)
    }

    pub(super) fn runtime_core(&self) -> &ThreadCore {
        &self.runtime_core
    }

    pub(crate) fn runtime_core_arc(&self) -> &Arc<ThreadCore> {
        &self.runtime_core
    }

    pub(super) fn grub_reclaimed_ns(
        &self,
        runtime_ns: u64,
        inactive_bw_scaled: u64,
        extra_bw_scaled: u64,
        max_bw_scaled: u64,
    ) -> u64 {
        // A PI owner may execute on a different CPU from the Deadline donor.
        // Its local GRUB snapshot therefore does not describe the donor's root
        // domain. Conservatively debit wall time until a coherent root-domain
        // bandwidth snapshot can be passed with the CBS baton.
        if self.deadline_donor.is_some() {
            return 0;
        }
        let SchedulePolicy::Deadline(policy) = self.policy else {
            return 0;
        };
        if !policy.flags().contains(crate::DeadlineFlags::RECLAIM) || max_bw_scaled == 0 {
            return 0;
        }
        let own_bw_scaled = DeadlineAdmission::utilization(policy);
        if own_bw_scaled > max_bw_scaled {
            task_runtime::fatal_invariant(0x444c_1011, self.thread.as_u64() as usize);
        }
        let charged_ns = grub_charge_ns(
            runtime_ns,
            own_bw_scaled,
            inactive_bw_scaled,
            extra_bw_scaled,
            max_bw_scaled,
        );
        runtime_ns - charged_ns
    }

    pub(super) fn is_rt(&self) -> bool {
        matches!(
            self.policy,
            SchedulePolicy::Fifo { .. } | SchedulePolicy::RoundRobin { .. }
        )
    }

    pub(crate) const fn schedule_snapshot(&self, role: DispatchRole) -> CurrentSchedule {
        CurrentSchedule {
            thread: self.thread,
            policy: self.policy,
            entity: self.entity,
            role,
        }
    }

    pub(crate) const fn schedule_policy(&self) -> SchedulePolicy {
        self.policy
    }

    pub(super) fn next_scheduler_event_ns(&self, now_ns: u64) -> Option<u64> {
        match self.entity {
            SchedulingEntity::KernelStop => None,
            SchedulingEntity::Fair(fair) => {
                Some(scheduler_time_advance(now_ns, fair.remaining_request_ns()))
            }
            SchedulingEntity::Fifo => None,
            SchedulingEntity::RoundRobin {
                remaining_quantum_ns,
            } => Some(scheduler_time_advance(now_ns, remaining_quantum_ns)),
            SchedulingEntity::Deadline(deadline) => {
                let mut next = deadline.next_scheduler_event_ns();
                if !self.pi_critical_rescue {
                    next = earliest(
                        next,
                        scheduler_time_advance(now_ns, deadline.remaining_runtime_ns()),
                    );
                }
                next
            }
        }
    }
}

fn grub_charge_ns(
    runtime_ns: u64,
    own_bw_scaled: u64,
    inactive_bw_scaled: u64,
    extra_bw_scaled: u64,
    max_bw_scaled: u64,
) -> u64 {
    assert!(max_bw_scaled > 0);
    assert!(own_bw_scaled <= max_bw_scaled);

    // Linux compares Uinact + Uextra against Umax - u instead of
    // subtracting first: the reclaimable sum may legitimately exceed Umax.
    let reclaimable_bw_scaled = inactive_bw_scaled as u128 + extra_bw_scaled as u128;
    let charge_rate_scaled = if reclaimable_bw_scaled > (max_bw_scaled - own_bw_scaled) as u128 {
        own_bw_scaled
    } else {
        max_bw_scaled - reclaimable_bw_scaled as u64
    };
    let charged_ns = runtime_ns as u128 * charge_rate_scaled as u128 / max_bw_scaled as u128;
    u64::try_from(charged_ns).expect("GRUB charge cannot exceed the supplied runtime")
}

fn deadline_key(entity: SchedulingEntity) -> u64 {
    entity
        .deadline()
        .and_then(DeadlineEntity::absolute_deadline_ns)
        .expect("a runnable Deadline entity must own an absolute deadline")
}

/// Result of one allocation-free local dispatch charge.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct DispatchCharge {
    pub(crate) slice_expired: bool,
    pub(crate) deadline_overrun: bool,
}

#[cfg(test)]
mod tests {
    use super::grub_charge_ns;

    #[test]
    fn grub_charge_uses_linux_fixed_point_truncation() {
        assert_eq!(grub_charge_ns(1, 1, 1, 0, 2), 0);
    }
}
