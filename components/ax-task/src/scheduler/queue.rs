//! Per-CPU runqueue protected by the target CPU's IRQ-safe scheduler lock.

use alloc::{sync::Arc, vec::Vec};

mod class;
mod deadline;
mod realtime;
mod task;

pub(crate) use class::{SchedulerClass, wakeup_preempts};
use deadline::{DeadlineQueueKey, DeadlineRunQueue};
use realtime::RealtimeRunQueue;
pub(crate) use task::{
    PickedThread, QueuedThread, QueuedThreadSnapshot, RqTaskMetadata, RunQueueNodeStorage,
};

use super::fair_queue::FairRunQueue;
#[cfg(test)]
use crate::ActiveSchedulingState;
use crate::{
    CurrentDispatch, DispatchCharge, FairEntity, FairMode, SchedulePolicy, SchedulingEntity,
    TaskError, ThreadCore, ThreadId,
};

#[cfg(test)]
std::thread_local! {
    static FAIR_RUNQUEUE_VISITS: core::cell::Cell<usize> = const {
        core::cell::Cell::new(0)
    };
    static RUNQUEUE_MEMBERSHIP_LOOKUPS: core::cell::Cell<usize> = const {
        core::cell::Cell::new(0)
    };
    static DEADLINE_RUNQUEUE_VISITS: core::cell::Cell<usize> = const {
        core::cell::Cell::new(0)
    };
}

#[cfg(test)]
fn reset_fair_runqueue_visits() {
    FAIR_RUNQUEUE_VISITS.set(0);
}

#[cfg(test)]
fn fair_runqueue_visits() -> usize {
    FAIR_RUNQUEUE_VISITS.get()
}

#[cfg(test)]
pub(super) fn record_fair_runqueue_visit() {
    FAIR_RUNQUEUE_VISITS.set(FAIR_RUNQUEUE_VISITS.get().saturating_add(1));
}

#[cfg(test)]
fn reset_runqueue_membership_lookups() {
    RUNQUEUE_MEMBERSHIP_LOOKUPS.set(0);
}

#[cfg(test)]
fn runqueue_membership_lookups() -> usize {
    RUNQUEUE_MEMBERSHIP_LOOKUPS.get()
}

#[cfg(test)]
fn reset_deadline_runqueue_visits() {
    DEADLINE_RUNQUEUE_VISITS.set(0);
}

#[cfg(test)]
fn deadline_runqueue_visits() -> usize {
    DEADLINE_RUNQUEUE_VISITS.get()
}

#[cfg(test)]
fn record_deadline_runqueue_visit() {
    DEADLINE_RUNQUEUE_VISITS.set(DEADLINE_RUNQUEUE_VISITS.get().saturating_add(1));
}

/// Why a runnable thread is being inserted into its owner run queue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnqueueReason {
    /// Newly ready or awakened work joins the class tail.
    Wake,
    /// An explicit yield joins the class tail.
    Yield,
    /// Higher-class preemption preserves FIFO/RR position.
    Preempted,
    /// A replenished reservation becomes eligible again.
    Replenished,
    /// Runnable state was handed off by another owner CPU without a new wake.
    Migrated,
    /// The owner CPU applied a newer scheduling-policy generation.
    PolicyChanged,
}

/// Which fixed-priority RT entities are eligible in this owner selection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RtEligibility {
    /// Linux `rt_rq_throttled()` is false for this runqueue.
    Runnable,
    /// No boosted entity keeps this runqueue runnable after quota exhaustion.
    Throttled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum QueueMembershipClass {
    Stop,
    Deadline(DeadlineQueueKey),
    /// Linux `p->on_rq == TASK_ON_RQ_QUEUED` while `dl_throttled` keeps the
    /// entity outside `dl_rq->root` and `rq->nr_running`.
    DeadlineThrottled,
    Realtime(u8),
    Fair,
    IdleFair,
}

impl QueueMembershipClass {
    const fn scheduler_class(self) -> SchedulerClass {
        match self {
            Self::Stop => SchedulerClass::Stop,
            Self::Deadline(_) | Self::DeadlineThrottled => SchedulerClass::Deadline,
            Self::Realtime(_) => SchedulerClass::Realtime,
            Self::Fair => SchedulerClass::Fair,
            Self::IdleFair => SchedulerClass::IdleFair,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct QueueMembership {
    generation: u32,
    class: QueueMembershipClass,
}

#[derive(Debug)]
pub(crate) struct RunQueue {
    /// Linux `rq->curr`: the sole running-task identity owned by this rq.
    ///
    /// RT/DL retain their class nodes while running, but those nodes do not
    /// carry a second "current" marker. Every class operation derives current
    /// status from this dispatch token while holding the rq lock.
    current: Option<CurrentDispatch>,
    stop: Option<QueuedThread>,
    deadline: DeadlineRunQueue,
    rt: RealtimeRunQueue,
    fair: FairRunQueue,
    idle_fair: FairRunQueue,
    membership: Vec<Option<QueueMembership>>,
    fixed_placement_demand: u64,
    balance_scan_epoch: u64,
    next_sequence: u64,
    /// Linux `rq->nr_running`: runnable non-idle tasks, including current.
    nr_running: usize,
}

impl RunQueue {
    pub(crate) fn configured(deadline_max_bw_scaled: u64, thread_capacity: usize) -> Self {
        Self {
            current: None,
            stop: None,
            deadline: DeadlineRunQueue::new(deadline_max_bw_scaled, thread_capacity),
            rt: RealtimeRunQueue::new(),
            fair: FairRunQueue::new(),
            idle_fair: FairRunQueue::new(),
            membership: Vec::new(),
            fixed_placement_demand: 0,
            balance_scan_epoch: 0,
            next_sequence: 0,
            nr_running: 0,
        }
    }

    #[cfg(test)]
    fn new() -> Self {
        Self::configured(u64::MAX, 64)
    }

    pub(crate) const fn current(&self) -> Option<&CurrentDispatch> {
        self.current.as_ref()
    }

    pub(crate) fn current_mut(&mut self) -> Option<&mut CurrentDispatch> {
        self.current.as_mut()
    }

    pub(crate) fn install_current(&mut self, current: CurrentDispatch) {
        assert!(
            self.current.replace(current).is_none(),
            "rq->curr must be cleared before installing a successor"
        );
    }

    pub(crate) fn take_current(&mut self) -> Option<CurrentDispatch> {
        self.current.take()
    }

    fn linked_current(&self) -> Option<ThreadId> {
        let current = self.current.as_ref()?.thread();
        matches!(
            self.membership_class(current),
            Some(QueueMembershipClass::Deadline(_) | QueueMembershipClass::Realtime(_))
        )
        .then_some(current)
    }

    /// Charges `rq->curr` and its class-owned entity in one rq transaction.
    ///
    /// The common dispatch token and RT/DL active nodes are disjoint fields
    /// of the same rq. Keeping this operation here prevents callers from
    /// temporarily clearing `rq->curr` merely to obtain two mutable borrows.
    pub(crate) fn charge_current(
        &mut self,
        runtime_ns: u64,
        now_ns: u64,
        inactive_bw_scaled: u64,
        extra_bw_scaled: u64,
        max_bw_scaled: u64,
        reclaimed_ns: u64,
    ) -> Result<(DispatchCharge, SchedulePolicy, SchedulingEntity, bool), TaskError> {
        let current = self.current.as_ref().ok_or(TaskError::NoRunnableThread)?;
        let id = current.thread();
        let policy = current.schedule_policy();
        let rt_quota_exempt = current.rt_quota_exempt();
        let membership = self.membership_class(id);
        let current_entity = match membership {
            Some(QueueMembershipClass::Deadline(key)) => self
                .deadline
                .get(key)
                .map(QueuedThread::entity)
                .ok_or(TaskError::InvalidConfiguration)?,
            Some(QueueMembershipClass::Realtime(priority)) => self
                .rt
                .get(priority, id)
                .map(QueuedThread::entity)
                .ok_or(TaskError::InvalidConfiguration)?,
            _ => current
                .owned_scheduling_entity()
                .ok_or(TaskError::InvalidConfiguration)?,
        };
        let dispatch = self.current.as_mut().ok_or(TaskError::NoRunnableThread)?;
        let grub_reclaimed_ns = dispatch.grub_reclaimed_ns(
            &current_entity,
            runtime_ns,
            inactive_bw_scaled,
            extra_bw_scaled,
            max_bw_scaled,
        );
        let reclaimed_ns = reclaimed_ns.saturating_add(grub_reclaimed_ns);
        let charge = match membership {
            Some(QueueMembershipClass::Deadline(key)) => {
                let entity = &mut self
                    .deadline
                    .get_mut(key)
                    .ok_or(TaskError::InvalidConfiguration)?
                    .active
                    .entity_mut();
                dispatch.charge_linked(entity, runtime_ns, now_ns, reclaimed_ns)
            }
            Some(QueueMembershipClass::Realtime(priority)) => {
                let entity = &mut self
                    .rt
                    .get_mut(priority, id)
                    .ok_or(TaskError::InvalidConfiguration)?
                    .active
                    .entity_mut();
                dispatch.charge_linked(entity, runtime_ns, now_ns, reclaimed_ns)
            }
            _ => dispatch.charge(runtime_ns, now_ns, reclaimed_ns),
        };
        let charged_entity = match membership {
            Some(QueueMembershipClass::Deadline(key)) => self
                .deadline
                .get(key)
                .map(QueuedThread::entity)
                .ok_or(TaskError::InvalidConfiguration)?,
            Some(QueueMembershipClass::Realtime(priority)) => self
                .rt
                .get(priority, id)
                .map(QueuedThread::entity)
                .ok_or(TaskError::InvalidConfiguration)?,
            _ => self
                .current
                .as_ref()
                .and_then(CurrentDispatch::owned_scheduling_entity)
                .ok_or(TaskError::InvalidConfiguration)?,
        };
        Ok((charge, policy, charged_entity, rt_quota_exempt))
    }

    /// Reserves every class index before a thread becomes externally visible.
    /// Scheduler fast paths treat missing capacity as an invariant violation
    /// instead of allocating under the irqsave rq lock.
    pub(crate) fn prepare_thread_slot(&mut self, slot: usize) {
        if self.membership.len() <= slot {
            self.membership.resize(slot.saturating_add(1), None);
        }
        self.deadline.prepare_thread_slot(slot);
        self.fair.prepare_thread_slot(slot);
        self.idle_fair.prepare_thread_slot(slot);
    }

    pub(crate) const fn nr_running(&self) -> usize {
        self.nr_running
    }

    pub(crate) fn nr_queued(&self) -> usize {
        let current_runnable = self
            .current
            .as_ref()
            .is_some_and(|current| !current.is_dedicated_idle());
        self.nr_running
            .checked_sub(usize::from(current_runnable))
            .expect("rq->curr runnable state must be included in rq->nr_running")
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.nr_queued()
    }

    /// Deactivates a Fair/stop current whose entity is intentionally outside
    /// every active class structure while it runs.
    pub(crate) fn deactivate_unlinked_current(&mut self, id: ThreadId) {
        assert!(
            !self.contains(id),
            "an rq-linked current must be deactivated through its class"
        );
        self.nr_running = self
            .nr_running
            .checked_sub(1)
            .expect("current deactivation must match one runnable entity");
    }

    pub(crate) fn fair_demand(&self) -> u64 {
        self.fair
            .total_weight()
            .saturating_add(self.idle_fair.total_weight())
    }

    pub(crate) fn placement_demand(&self) -> u64 {
        self.fair_demand()
            .saturating_add(self.fixed_placement_demand)
    }

    #[cfg(test)]
    pub(crate) const fn virtual_time(&self) -> u64 {
        self.fair.virtual_time()
    }

    #[cfg(test)]
    pub(crate) fn set_virtual_time_for_test(&mut self, virtual_time: u64) {
        self.fair.set_virtual_time_for_test(virtual_time);
    }

    pub(crate) const fn virtual_time_for_mode(&self, mode: FairMode) -> u64 {
        if matches!(mode, FairMode::Idle) {
            self.idle_fair.virtual_time()
        } else {
            self.fair.virtual_time()
        }
    }

    /// Updates each fair class's authoritative weighted-average virtual time.
    ///
    /// `current` is supplied because the running entity is temporarily absent
    /// from the owner runqueue. Like Linux `avg_vruntime()`, insertion and
    /// removal may move this average in either direction; saved `vlag` protects
    /// entities from those membership changes.
    pub(crate) fn update_fair_virtual_time(&mut self, current: Option<FairEntity>) {
        let normal_current = current.filter(|entity| entity.mode() != FairMode::Idle);
        let idle_current = current.filter(|entity| entity.mode() == FairMode::Idle);
        self.fair.update_virtual_time(normal_current);
        self.idle_fair.update_virtual_time(idle_current);
    }

    pub(crate) fn has_rt(&self) -> bool {
        self.rt.has_any_rt()
    }

    pub(crate) fn has_exempt_rt(&self) -> bool {
        self.rt.has_exempt_rt()
    }

    pub(crate) fn highest_rt_priority(&self) -> Option<u8> {
        self.rt.highest_rt_priority()
    }

    pub(crate) fn rt_count_at_priority(&self, priority: u8) -> usize {
        self.rt.count_at_priority(priority)
    }

    pub(crate) fn has_fair(&self) -> bool {
        !self.fair.is_empty()
    }

    pub(crate) fn has_idle_fair(&self) -> bool {
        !self.idle_fair.is_empty()
    }

    pub(crate) fn fair_wakee_is_selected(
        &self,
        wakee: ThreadId,
        mode: FairMode,
        virtual_time: u64,
    ) -> bool {
        let queue = if mode == FairMode::Idle {
            &self.idle_fair
        } else {
            &self.fair
        };
        queue.earliest_eligible(virtual_time) == Some(wakee)
    }

    pub(crate) fn earliest_deadline_ns(&self) -> Option<u64> {
        self.deadline.earliest_deadline_ns()
    }

    pub(crate) fn deadline_members_are_empty(&self) -> bool {
        self.deadline.members_are_empty()
    }

    pub(crate) fn deadline_member(&self, thread: ThreadId) -> Option<Arc<ThreadCore>> {
        self.deadline.member(thread)
    }

    pub(crate) fn register_deadline_member(&mut self, core: &Arc<ThreadCore>) -> bool {
        self.deadline.register_member(core)
    }

    pub(crate) fn unregister_deadline_member(&mut self, core: &Arc<ThreadCore>) {
        self.deadline.unregister_member(core);
    }

    pub(crate) fn add_deadline_bandwidth(&mut self, utilization_scaled: u64, active: bool) {
        self.deadline.add_bandwidth(utilization_scaled, active);
    }

    pub(crate) fn remove_deadline_bandwidth(&mut self, utilization_scaled: u64, active: bool) {
        self.deadline.remove_bandwidth(utilization_scaled, active);
    }

    pub(crate) fn activate_deadline_bandwidth(&mut self, utilization_scaled: u64) {
        self.deadline.activate_bandwidth(utilization_scaled);
    }

    pub(crate) fn deactivate_deadline_bandwidth(&mut self, utilization_scaled: u64) {
        self.deadline.deactivate_bandwidth(utilization_scaled);
    }

    pub(crate) const fn deadline_bandwidth(&self) -> crate::DeadlineBandwidthSnapshot {
        self.deadline.bandwidth()
    }

    pub(crate) fn has_pushable_deadline(&self) -> bool {
        self.deadline.has_pushable()
    }

    pub(crate) fn has_pushable_realtime(&self) -> bool {
        self.rt.has_pushable()
    }

    pub(crate) fn has_pushable_fair(&self) -> bool {
        self.fair.has_migratable()
    }

    fn refresh_class_pushable(
        &mut self,
        thread: ThreadId,
        policy: SchedulePolicy,
        current: Option<ThreadId>,
    ) {
        match policy {
            SchedulePolicy::Deadline(_) => self.deadline.refresh_pushable(thread, current),
            SchedulePolicy::Fifo { priority } | SchedulePolicy::RoundRobin { priority, .. } => {
                self.rt.refresh_pushable_priority(priority.get(), current);
            }
            SchedulePolicy::KernelStop | SchedulePolicy::Fair { .. } => {}
        }
    }

    /// Updates the configured-policy Deadline server retained inside a task.
    ///
    /// When a Deadline PI entity owns the active EDF key, its base CBS is
    /// parked inside the same rq-owned scheduling state. Updating that base
    /// server must not rebuild the donor's active key.
    pub(crate) fn update_base_deadline_entity(
        &mut self,
        id: ThreadId,
        entity: SchedulingEntity,
    ) -> bool {
        let Some(class) = self.membership_class(id) else {
            return false;
        };
        match class {
            QueueMembershipClass::Deadline(key) => {
                let Some(thread) = self.deadline.get_mut(key) else {
                    return false;
                };
                if thread.active.uses_inherited_entity() {
                    thread.active.replace_base_entity(entity);
                    return true;
                }
                let Some(new_key) = self.deadline.update_entity(key, entity) else {
                    return false;
                };
                self.replace_membership_class(id, QueueMembershipClass::Deadline(new_key));
                self.deadline.refresh_pushable(id, self.linked_current());
                true
            }
            QueueMembershipClass::DeadlineThrottled => {
                let Some(thread) = self.deadline.throttled_mut(id) else {
                    return false;
                };
                thread.active.replace_base_entity(entity);
                true
            }
            _ => false,
        }
    }

    pub(crate) fn update_migration_capability(
        &mut self,
        id: ThreadId,
        migration_capable: bool,
    ) -> bool {
        let Some(class) = self.membership_class(id) else {
            return false;
        };
        match class {
            QueueMembershipClass::Stop => {
                self.stop
                    .as_mut()
                    .expect("stop membership must retain the stopper task")
                    .migration_capable = false;
            }
            QueueMembershipClass::Deadline(key) => {
                self.deadline
                    .get_mut(key)
                    .expect("Deadline membership must retain its queue node")
                    .migration_capable = migration_capable;
                self.deadline.refresh_pushable(id, self.linked_current());
            }
            QueueMembershipClass::DeadlineThrottled => {
                self.deadline
                    .throttled_mut(id)
                    .expect("throttled Deadline membership must retain its entity")
                    .migration_capable = migration_capable;
            }
            QueueMembershipClass::Realtime(priority) => {
                self.rt
                    .get_mut(priority, id)
                    .expect("RT membership must retain its queue node")
                    .migration_capable = migration_capable;
                self.rt
                    .refresh_pushable_priority(priority, self.linked_current());
            }
            QueueMembershipClass::Fair => {
                let mut thread = self
                    .fair
                    .remove(id)
                    .expect("fair membership must retain its queue node");
                thread.migration_capable = migration_capable;
                self.fair.insert(thread);
            }
            QueueMembershipClass::IdleFair => {
                let mut thread = self
                    .idle_fair
                    .remove(id)
                    .expect("idle-fair membership must retain its queue node");
                thread.migration_capable = migration_capable;
                self.idle_fair.insert(thread);
            }
        }
        true
    }

    pub(crate) fn begin_balance_scan(&mut self) -> u64 {
        self.balance_scan_epoch = self
            .balance_scan_epoch
            .checked_add(1)
            .expect("runqueue balance scan epoch must not wrap");
        self.balance_scan_epoch
    }

    pub(crate) fn next_balance_candidate(
        &mut self,
        scan_epoch: u64,
        mut may_migrate: impl FnMut(&QueuedThread) -> bool,
    ) -> Option<QueuedThreadSnapshot> {
        let linked_current = self.linked_current();
        let candidate = self
            .deadline
            .find_first_matching(&mut |thread| {
                Some(thread.id) != linked_current
                    && thread.balance_scan_epoch != scan_epoch
                    && may_migrate(thread)
            })
            .or_else(|| {
                self.rt.find_first_matching(&mut |thread| {
                    Some(thread.id) != linked_current
                        && thread.balance_scan_epoch != scan_epoch
                        && may_migrate(thread)
                })
            })
            .or_else(|| {
                self.fair.find_first_matching(&mut |thread| {
                    thread.balance_scan_epoch != scan_epoch && may_migrate(thread)
                })
            })?;
        self.mark_balance_candidate(candidate.id, scan_epoch);
        Some(candidate)
    }

    pub(crate) fn queued_thread(&self, id: ThreadId) -> Option<QueuedThreadSnapshot> {
        if self.linked_current() == Some(id) {
            return None;
        }
        match self.membership_class(id)? {
            QueueMembershipClass::Stop => self.stop.as_ref().map(QueuedThreadSnapshot::from),
            QueueMembershipClass::Deadline(key) => {
                self.deadline.get(key).map(QueuedThreadSnapshot::from)
            }
            QueueMembershipClass::DeadlineThrottled => None,
            QueueMembershipClass::Realtime(priority) => {
                self.rt.get(priority, id).map(QueuedThreadSnapshot::from)
            }
            QueueMembershipClass::Fair => {
                self.fair.find_first_matching(&mut |thread| thread.id == id)
            }
            QueueMembershipClass::IdleFair => self
                .idle_fair
                .find_first_matching(&mut |thread| thread.id == id),
        }
    }

    pub(crate) fn enqueue_task(
        &mut self,
        mut entry: QueuedThread,
        reason: EnqueueReason,
        current_fair: Option<FairEntity>,
    ) -> Result<SchedulingEntity, TaskError> {
        if self.contains(entry.id) {
            return Err(TaskError::AlreadyQueued);
        }
        entry.sequence = self.allocate_sequence();
        let id = entry.id;
        let policy = entry.active.policy();
        let class_enqueue =
            SchedulerClass::for_policy(policy).enqueue_task(self, entry, reason, current_fair)?;
        let membership_class = class_enqueue.membership;
        let queued_entity = class_enqueue.entity;
        let reason = class_enqueue.reason;
        if matches!(
            reason,
            EnqueueReason::Wake | EnqueueReason::Replenished | EnqueueReason::Migrated
        ) {
            self.nr_running += 1;
        }
        self.fixed_placement_demand = self
            .fixed_placement_demand
            .saturating_add(fixed_placement_demand(policy));
        self.register_membership(id, membership_class);
        self.refresh_class_pushable(id, policy, self.linked_current());
        Ok(queued_entity)
    }

    /// Activates an already-throttled Deadline task without linking it into
    /// the EDF tree. Linux publishes `TASK_ON_RQ_QUEUED` in this state while
    /// leaving the task out of `rq->nr_running` until the CBS timer fires.
    pub(crate) fn enqueue_throttled_deadline(
        &mut self,
        mut thread: QueuedThread,
    ) -> Result<(), TaskError> {
        if self.contains(thread.id)
            || !matches!(thread.active.policy(), SchedulePolicy::Deadline(_))
            || !thread.active.entity().is_deadline_throttled()
        {
            return Err(TaskError::InvalidConfiguration);
        }
        thread.sequence = self.allocate_sequence();
        let id = thread.id;
        self.deadline.install_throttled(thread)?;
        self.register_membership(id, QueueMembershipClass::DeadlineThrottled);
        Ok(())
    }

    /// Linux `update_curr_dl()` throttle transition for the linked current.
    pub(crate) fn throttle_current_deadline(
        &mut self,
        id: ThreadId,
    ) -> Result<SchedulingEntity, TaskError> {
        if self.linked_current() != Some(id) {
            return Err(TaskError::NotReady);
        }
        let QueueMembershipClass::Deadline(key) =
            self.membership_class(id).ok_or(TaskError::NotReady)?
        else {
            return Err(TaskError::InvalidConfiguration);
        };
        let thread = self.deadline.remove(key).ok_or(TaskError::NotReady)?;
        let entity = thread.active.entity().clone();
        self.deadline.install_throttled(thread)?;
        self.replace_membership_class(id, QueueMembershipClass::DeadlineThrottled);
        self.nr_running = self
            .nr_running
            .checked_sub(1)
            .ok_or(TaskError::InvalidConfiguration)?;
        Ok(entity)
    }

    /// Re-enables one throttled CBS entity after its hard replenishment timer.
    pub(crate) fn replenish_throttled_deadline(
        &mut self,
        id: ThreadId,
        entity: SchedulingEntity,
    ) -> Result<(), TaskError> {
        if !matches!(
            self.membership_class(id),
            Some(QueueMembershipClass::DeadlineThrottled)
        ) || entity.is_deadline_throttled()
        {
            return Err(TaskError::NotReady);
        }
        let mut thread = self
            .deadline
            .take_throttled(id)
            .ok_or(TaskError::NotReady)?;
        *thread.active.entity_mut() = entity;
        let policy = thread.active.policy();
        let key = self.deadline.insert(thread);
        self.replace_membership_class(id, QueueMembershipClass::Deadline(key));
        self.nr_running = self
            .nr_running
            .checked_add(1)
            .ok_or(TaskError::InvalidConfiguration)?;
        self.fixed_placement_demand = self
            .fixed_placement_demand
            .saturating_add(fixed_placement_demand(policy));
        self.deadline.refresh_pushable(id, self.linked_current());
        Ok(())
    }

    pub(crate) fn is_deadline_throttled_member(&self, id: ThreadId) -> bool {
        self.membership_class(id) == Some(QueueMembershipClass::DeadlineThrottled)
    }

    fn fair_placement_weights(
        &self,
        fair: FairEntity,
        current_fair: Option<FairEntity>,
    ) -> (u64, u64) {
        let queue_weight = if fair.mode() == FairMode::Idle {
            self.idle_fair.total_weight()
        } else {
            self.fair.total_weight()
        };
        let current_weight = current_fair
            .filter(|current| current.mode() == fair.mode())
            .map_or(0, |current| u64::from(current.weight()));
        (queue_weight, current_weight)
    }

    fn mark_balance_candidate(&mut self, id: ThreadId, scan_epoch: u64) {
        match self
            .membership_class(id)
            .expect("a selected balance candidate must remain queued")
        {
            QueueMembershipClass::Stop => {
                unreachable!("the per-CPU stopper must never be a balance candidate")
            }
            QueueMembershipClass::Deadline(key) => {
                self.deadline
                    .get_mut(key)
                    .expect("deadline balance candidate must remain linked")
                    .balance_scan_epoch = scan_epoch;
            }
            QueueMembershipClass::DeadlineThrottled => {
                unreachable!("a throttled Deadline task is not a push candidate")
            }
            QueueMembershipClass::Realtime(priority) => {
                self.rt
                    .get_mut(priority, id)
                    .expect("RT balance candidate must remain linked")
                    .balance_scan_epoch = scan_epoch;
            }
            QueueMembershipClass::Fair => {
                let mut thread = self
                    .fair
                    .remove(id)
                    .expect("fair balance candidate must remain linked");
                thread.balance_scan_epoch = scan_epoch;
                self.fair.insert(thread);
            }
            QueueMembershipClass::IdleFair => {
                let mut thread = self
                    .idle_fair
                    .remove(id)
                    .expect("idle-fair balance candidate must remain linked");
                thread.balance_scan_epoch = scan_epoch;
                self.idle_fair.insert(thread);
            }
        }
    }

    #[cfg(test)]
    fn enqueue_test(
        &mut self,
        id: ThreadId,
        policy: SchedulePolicy,
        entity: SchedulingEntity,
        _now_ns: u64,
        reason: EnqueueReason,
    ) -> Result<SchedulingEntity, TaskError> {
        self.prepare_thread_slot(id.slot() as usize);
        let sched = Arc::new(crate::ThreadSchedCell::new_test(id, policy));
        let core = Arc::new(ThreadCore::new(id, policy, sched, None, None, None));
        let already_runnable = matches!(reason, EnqueueReason::Preempted);
        let entity = self.enqueue_task(
            QueuedThread::new(
                id,
                ActiveSchedulingState::new(policy, entity),
                core,
                false,
                true,
                RqTaskMetadata::test(1),
            ),
            reason,
            None,
        )?;
        if already_runnable {
            // Production reaches `Preempted` from a Fair current which is
            // already included in rq->nr_running. Unit tests inject that
            // post-put-prev state directly, so establish the same common-rq
            // accounting without reapplying the wake placement rule.
            self.nr_running = self
                .nr_running
                .checked_add(1)
                .ok_or(TaskError::InvalidConfiguration)?;
        }
        Ok(entity)
    }

    #[cfg(test)]
    fn enqueue_rt_test(
        &mut self,
        id: ThreadId,
        policy: SchedulePolicy,
        quota_exempt: bool,
    ) -> Result<SchedulingEntity, TaskError> {
        self.prepare_thread_slot(id.slot() as usize);
        let sched = Arc::new(crate::ThreadSchedCell::new_test(id, policy));
        let core = Arc::new(ThreadCore::new(id, policy, sched, None, None, None));
        self.enqueue_task(
            QueuedThread::new(
                id,
                ActiveSchedulingState::new(policy, SchedulingEntity::new(policy, 1, 0)),
                core,
                quota_exempt,
                true,
                RqTaskMetadata::test(1),
            ),
            EnqueueReason::Wake,
            None,
        )
    }

    fn unlink_task(&mut self, id: ThreadId, deactivate: bool) -> Option<QueuedThread> {
        let class = self.membership_class(id)?;
        if class == QueueMembershipClass::DeadlineThrottled {
            let thread = self.deadline.take_throttled(id)?;
            self.unregister_membership(id);
            return Some(thread);
        }
        let was_linked_current = self.linked_current() == Some(id);
        let scheduler_class = class.scheduler_class();
        let removed = scheduler_class
            .dequeue_task(self, class, id)
            .expect("runqueue membership must identify a linked scheduling entity");
        if !was_linked_current {
            self.fixed_placement_demand = self
                .fixed_placement_demand
                .saturating_sub(fixed_placement_demand(removed.active.policy()));
        }
        self.unregister_membership(removed.id);
        self.refresh_class_pushable(removed.id, removed.active.policy(), self.linked_current());
        if deactivate {
            self.nr_running = self
                .nr_running
                .checked_sub(1)
                .expect("deactivate_task must match one runnable entity");
        }
        Some(removed)
    }

    /// Linux `deactivate_task()`: removes one runnable entity from `nr_running`.
    pub(crate) fn deactivate_task(&mut self, id: ThreadId) -> Option<QueuedThread> {
        self.unlink_task(id, true)
    }

    /// Linux scheduler-class change: unlinks an entity without deactivating it.
    pub(crate) fn reclassify_task(&mut self, id: ThreadId) -> Option<QueuedThread> {
        let was_throttled =
            self.membership_class(id) == Some(QueueMembershipClass::DeadlineThrottled);
        let thread = self.unlink_task(id, false)?;
        if was_throttled {
            // The replacement class becomes eligible immediately. Common
            // PolicyChanged enqueue preserves `nr_running`, so establish the
            // runnable count here just as Linux dequeues a throttled DL class
            // before installing the new scheduler class.
            self.nr_running = self.nr_running.checked_add(1)?;
        }
        Some(thread)
    }

    #[cfg(test)]
    fn dequeue(&mut self, id: ThreadId) -> Option<QueuedThread> {
        self.deactivate_task(id)
    }

    /// Returns whether `id` is the RT/DL entity retained as current.
    pub(crate) fn is_linked_current(&self, id: ThreadId) -> bool {
        self.linked_current() == Some(id)
    }

    pub(crate) fn linked_current_entity_mut(
        &mut self,
        id: ThreadId,
    ) -> Option<&mut SchedulingEntity> {
        if self.linked_current() != Some(id) {
            return None;
        }
        match self.membership_class(id)? {
            QueueMembershipClass::Deadline(key) => {
                Some(self.deadline.get_mut(key)?.active.entity_mut())
            }
            QueueMembershipClass::Realtime(priority) => {
                Some(self.rt.get_mut(priority, id)?.active.entity_mut())
            }
            _ => None,
        }
    }

    pub(crate) fn capture_linked_fair_migration(
        &mut self,
        id: ThreadId,
        virtual_time: u64,
        timing_granularity_ns: u64,
    ) -> bool {
        let active = match self.membership_class(id) {
            Some(QueueMembershipClass::Deadline(key)) => {
                self.deadline.get_mut(key).map(|thread| &mut thread.active)
            }
            Some(QueueMembershipClass::Realtime(priority)) => self
                .rt
                .get_mut(priority, id)
                .map(|thread| &mut thread.active),
            _ => None,
        };
        let Some(active) = active else {
            return false;
        };
        active
            .base_entity_mut()
            .capture_fair_migration(virtual_time, timing_granularity_ns);
        true
    }

    pub(crate) fn linked_current_entity(&self, id: ThreadId) -> Option<SchedulingEntity> {
        if self.linked_current() != Some(id) {
            return None;
        }
        self.queued_thread_including_current(id)
            .map(|thread| thread.entity)
    }

    /// Rebuilds the active EDF key after Linux-style boosted replenishment.
    pub(crate) fn requeue_replenished_deadline_current(
        &mut self,
        id: ThreadId,
    ) -> Result<bool, TaskError> {
        if self.linked_current() != Some(id) {
            return Err(TaskError::NotReady);
        }
        let QueueMembershipClass::Deadline(key) =
            self.membership_class(id).ok_or(TaskError::NotReady)?
        else {
            return Err(TaskError::InvalidConfiguration);
        };
        let (new_key, _entity) = self
            .deadline
            .put_prev_current(key)
            .ok_or(TaskError::NotReady)?;
        self.replace_membership_class(id, QueueMembershipClass::Deadline(new_key));
        Ok(self.deadline.first().is_some_and(|thread| thread.id != id))
    }

    pub(crate) fn scheduling_entity(&self, id: ThreadId) -> Option<SchedulingEntity> {
        self.queued_thread_including_current(id)
            .map(|thread| thread.entity)
    }

    pub(crate) fn base_scheduling_entity(&self, id: ThreadId) -> Option<SchedulingEntity> {
        self.queued_thread_including_current(id)
            .map(|thread| thread.base_entity)
    }

    pub(crate) fn scheduling_state(
        &self,
        id: ThreadId,
    ) -> Option<(SchedulePolicy, SchedulingEntity)> {
        self.queued_thread_including_current(id)
            .map(|thread| (thread.policy, thread.entity))
    }

    #[cfg(test)]
    pub(crate) fn debug_owns_schedule_state(&self, id: ThreadId) -> bool {
        self.queued_thread_including_current(id).is_some()
    }

    /// Installs a newly applied RT/DL policy as the physically linked current.
    pub(crate) fn link_running(&mut self, thread: QueuedThread) -> Result<(), TaskError> {
        if self
            .linked_current()
            .is_some_and(|current| self.contains(current))
            || !retains_running_link(thread.active.policy())
        {
            return Err(TaskError::InvalidConfiguration);
        }
        let id = thread.id;
        let policy = thread.active.policy();
        self.enqueue_task(thread, EnqueueReason::PolicyChanged, None)?;
        self.fixed_placement_demand = self
            .fixed_placement_demand
            .saturating_sub(fixed_placement_demand(policy));
        self.refresh_class_pushable(id, policy, Some(id));
        Ok(())
    }

    /// Restores a class pick whose owner-rq validation did not reach set-next.
    pub(crate) fn rollback_pick(&mut self, picked: PickedThread) {
        match picked {
            PickedThread::Linked(_) => {}
            PickedThread::Owned(mut thread) => {
                thread.active.entity_mut().cancel_fair_migration();
                SchedulerClass::for_policy(thread.active.policy()).rollback_pick(self, thread);
            }
        }
    }

    /// Makes the retained RT/DL current queued again without transferring its
    /// intrusive node or membership identity.
    pub(crate) fn put_prev_task(
        &mut self,
        id: ThreadId,
        reason: EnqueueReason,
    ) -> Result<SchedulingEntity, TaskError> {
        if self.linked_current() != Some(id) {
            return Err(TaskError::NotReady);
        }
        let class = self.membership_class(id).ok_or(TaskError::NotReady)?;
        let policy = self
            .queued_thread_including_current(id)
            .ok_or(TaskError::NotReady)?
            .policy;
        let entity = SchedulerClass::for_policy(policy).put_prev_task(self, class, id, reason)?;
        self.fixed_placement_demand = self
            .fixed_placement_demand
            .saturating_add(fixed_placement_demand(policy));
        self.refresh_class_pushable(id, policy, None);
        Ok(entity)
    }

    pub(crate) fn detach_for_transfer(
        &mut self,
        id: ThreadId,
        current_fair: Option<FairEntity>,
        timing_granularity_ns: u64,
    ) -> Option<QueuedThread> {
        if self.linked_current() == Some(id) {
            return None;
        }
        self.update_fair_virtual_time(current_fair);
        let class = self.membership_class(id)?;
        if class == QueueMembershipClass::DeadlineThrottled {
            // Linux keeps a throttled DL task at
            // `TASK_ON_RQ_QUEUED`, but it is absent from both the DL rb-tree
            // and `rq->nr_running`. Migration moves that queued ownership
            // without applying the ordinary runnable accounting below. The
            // destination preserves the same throttled membership until its
            // hard CBS timer replenishes it.
            let thread = self.deadline.take_throttled(id)?;
            self.unregister_membership(id);
            return Some(thread);
        }
        let thread = SchedulerClass::for_policy(self.queued_thread_including_current(id)?.policy())
            .migrate_task_rq(self, class, id, timing_granularity_ns)?;
        self.nr_running = self
            .nr_running
            .checked_sub(1)
            .expect("migration must detach one runnable entity");
        self.fixed_placement_demand = self
            .fixed_placement_demand
            .saturating_sub(fixed_placement_demand(thread.active.policy()));
        self.unregister_membership(thread.id);
        self.refresh_class_pushable(thread.id, thread.active.policy(), self.linked_current());
        self.update_fair_virtual_time(current_fair);
        Some(thread)
    }

    pub(crate) fn pick_next_task(&mut self, rt_eligibility: RtEligibility) -> Option<PickedThread> {
        SchedulerClass::PICK_ORDER
            .into_iter()
            .find_map(|class| class.pick_task(self, rt_eligibility))
    }

    /// Linux `set_next_task()`: commits one class pick as current.
    pub(crate) fn set_next_task(&mut self, picked: &PickedThread) {
        self.fixed_placement_demand = self
            .fixed_placement_demand
            .saturating_sub(fixed_placement_demand(picked.policy()));
        SchedulerClass::for_policy(picked.policy()).set_next_task(self, picked);
        self.refresh_class_pushable(picked.id(), picked.policy(), Some(picked.id()));
    }

    fn queued_thread_including_current(&self, id: ThreadId) -> Option<QueuedThreadSnapshot> {
        match self.membership_class(id)? {
            QueueMembershipClass::Stop => self.stop.as_ref().map(QueuedThreadSnapshot::from),
            QueueMembershipClass::Deadline(key) => {
                self.deadline.get(key).map(QueuedThreadSnapshot::from)
            }
            QueueMembershipClass::DeadlineThrottled => {
                self.deadline.throttled(id).map(QueuedThreadSnapshot::from)
            }
            QueueMembershipClass::Realtime(priority) => {
                self.rt.get(priority, id).map(QueuedThreadSnapshot::from)
            }
            QueueMembershipClass::Fair => {
                self.fair.find_first_matching(&mut |thread| thread.id == id)
            }
            QueueMembershipClass::IdleFair => self
                .idle_fair
                .find_first_matching(&mut |thread| thread.id == id),
        }
    }

    fn contains(&self, id: ThreadId) -> bool {
        self.membership_class(id).is_some()
    }

    fn membership_class(&self, id: ThreadId) -> Option<QueueMembershipClass> {
        #[cfg(test)]
        RUNQUEUE_MEMBERSHIP_LOOKUPS.set(RUNQUEUE_MEMBERSHIP_LOOKUPS.get().saturating_add(1));
        self.membership
            .get(id.slot() as usize)
            .and_then(|membership| *membership)
            .filter(|membership| membership.generation == id.generation())
            .map(|membership| membership.class)
    }

    fn register_membership(&mut self, id: ThreadId, class: QueueMembershipClass) {
        let slot = id.slot() as usize;
        assert!(
            self.membership.len() > slot,
            "thread construction must prepare owner rq membership"
        );
        assert!(
            self.membership[slot]
                .replace(QueueMembership {
                    generation: id.generation(),
                    class,
                })
                .is_none(),
            "runqueue membership must be unique"
        );
    }

    fn unregister_membership(&mut self, id: ThreadId) {
        let membership = self
            .membership
            .get_mut(id.slot() as usize)
            .and_then(Option::take)
            .expect("queued thread must retain owner membership until removal");
        assert_eq!(membership.generation, id.generation());
    }

    fn replace_membership_class(&mut self, id: ThreadId, class: QueueMembershipClass) {
        let membership = self
            .membership
            .get_mut(id.slot() as usize)
            .and_then(Option::as_mut)
            .expect("queued thread must retain owner membership during rekey");
        assert_eq!(membership.generation, id.generation());
        membership.class = class;
    }

    fn allocate_sequence(&mut self) -> u64 {
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.wrapping_add(1);
        sequence
    }
}

const fn fixed_placement_demand(policy: SchedulePolicy) -> u64 {
    policy
        .placement_demand()
        .saturating_sub(policy.fair_demand())
}

const fn retains_running_link(policy: SchedulePolicy) -> bool {
    matches!(
        policy,
        SchedulePolicy::Deadline(_)
            | SchedulePolicy::Fifo { .. }
            | SchedulePolicy::RoundRobin { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CurrentClassState, CurrentDispatch, CurrentDispatchState, CurrentSchedule, DeadlineFlags,
        DeadlinePolicy, FairEntity, FairMode, Nice, RqTaskTime, RtPriority,
    };

    fn pick_next(queue: &mut RunQueue, eligibility: RtEligibility) -> PickedThread {
        let picked = queue.pick_next_task(eligibility).unwrap();
        queue.set_next_task(&picked);
        picked
    }

    fn pick_linked_current(queue: &mut RunQueue) -> ThreadId {
        let picked = queue.pick_next_task(RtEligibility::Runnable).unwrap();
        assert!(
            matches!(picked, PickedThread::Linked(_)),
            "only RT and Deadline retain their running entity in the class structure"
        );
        queue.set_next_task(&picked);
        let metadata = picked.metadata().clone();
        let core = Arc::clone(picked.core());
        let thread = picked.id();
        let dispatch = CurrentDispatch::new(
            CurrentDispatchState {
                thread,
                schedule: CurrentClassState::Linked {
                    policy: picked.policy(),
                },
                deadline_donor: metadata.deadline_donor,
                rt_quota_exempt: picked.rt_quota_exempt(),
                deadline_bandwidth_scaled: metadata.deadline_bandwidth_scaled,
                policy_generation: metadata.policy_generation,
                runtime_binding: metadata.runtime_binding,
            },
            &core,
            RqTaskTime::test(0),
        );
        queue.install_current(dispatch);
        thread
    }

    #[test]
    fn fair_wakeup_preemption_requires_the_wakee_to_be_the_eevdf_pick() {
        let mut queue = RunQueue::new();
        let policy = SchedulePolicy::fair(Nice::ZERO, FairMode::Normal);
        let contender = ThreadId::from_parts(1, 1);
        let wakee = ThreadId::from_parts(2, 1);
        let contender_entity = FairEntity::test_state(Nice::ZERO, FairMode::Normal, 900, 1_000);
        let wakee_entity = FairEntity::test_state(Nice::ZERO, FairMode::Normal, 950, 1_100);
        queue
            .enqueue_test(
                contender,
                policy,
                SchedulingEntity::Fair(contender_entity),
                0,
                EnqueueReason::Preempted,
            )
            .unwrap();
        queue
            .enqueue_test(
                wakee,
                policy,
                SchedulingEntity::Fair(wakee_entity),
                0,
                EnqueueReason::Preempted,
            )
            .unwrap();
        let mut current_entity = FairEntity::test_state(Nice::ZERO, FairMode::Normal, 1_000, 1_200);
        assert!(current_entity.charge(1, 0));
        queue.update_fair_virtual_time(Some(current_entity));
        let virtual_time = queue.virtual_time_for_mode(FairMode::Normal);
        let current = CurrentSchedule::test_state(policy, SchedulingEntity::Fair(current_entity));

        assert!(queue.fair_wakee_is_selected(contender, FairMode::Normal, virtual_time));
        assert!(!queue.fair_wakee_is_selected(wakee, FairMode::Normal, virtual_time));
        assert!(wakee_entity.deadline_precedes(current_entity));
        let preempts_current =
            current.should_preempt(policy, SchedulingEntity::Fair(wakee_entity), virtual_time)
                && queue.fair_wakee_is_selected(wakee, FairMode::Normal, virtual_time);
        assert!(
            !preempts_current,
            "a wakee that loses the full EEVDF pick must not request preemption",
        );
    }

    #[test]
    fn deadline_precedes_rt_and_fair() {
        let mut queue = RunQueue::new();
        let fair = SchedulePolicy::fair(Nice::ZERO, FairMode::Normal);
        let rt = SchedulePolicy::fifo(RtPriority::new(99).unwrap());
        let deadline =
            SchedulePolicy::deadline(DeadlinePolicy::new(1, 2, 3, DeadlineFlags::NONE).unwrap());
        queue
            .enqueue_test(
                ThreadId::from_parts(0, 1),
                fair,
                SchedulingEntity::new(fair, 1, 0),
                0,
                EnqueueReason::Wake,
            )
            .unwrap();
        queue
            .enqueue_test(
                ThreadId::from_parts(1, 1),
                rt,
                SchedulingEntity::new(rt, 1, 0),
                0,
                EnqueueReason::Wake,
            )
            .unwrap();
        let mut deadline_entity = SchedulingEntity::new(deadline, 1, 0);
        deadline_entity.activate_deadline(0);
        queue
            .enqueue_test(
                ThreadId::from_parts(2, 1),
                deadline,
                deadline_entity,
                0,
                EnqueueReason::Wake,
            )
            .unwrap();
        assert_eq!(
            pick_next(&mut queue, RtEligibility::Runnable).id(),
            ThreadId::from_parts(2, 1)
        );
    }

    #[test]
    fn deadline_runqueue_orders_across_linux_rq_clock_wrap() {
        let mut queue = RunQueue::new();
        let earlier_policy =
            SchedulePolicy::deadline(DeadlinePolicy::new(1, 4, 20, DeadlineFlags::NONE).unwrap());
        let later_policy =
            SchedulePolicy::deadline(DeadlinePolicy::new(1, 10, 20, DeadlineFlags::NONE).unwrap());
        let earlier_id = ThreadId::from_parts(0, 1);
        let later_id = ThreadId::from_parts(1, 1);
        let now = u64::MAX - 5;

        for (id, policy) in [(later_id, later_policy), (earlier_id, earlier_policy)] {
            let mut entity = SchedulingEntity::new(policy, 1, 0);
            entity.activate_deadline(now);
            queue
                .enqueue_test(id, policy, entity, now, EnqueueReason::Wake)
                .unwrap();
        }

        assert_eq!(
            pick_next(&mut queue, RtEligibility::Runnable).id(),
            earlier_id
        );
    }

    #[test]
    fn kernel_stopper_runs_before_deadline_even_when_rt_is_throttled() {
        let mut queue = RunQueue::new();
        let stopper = SchedulePolicy::kernel_stop();
        let deadline =
            SchedulePolicy::deadline(DeadlinePolicy::new(1, 2, 3, DeadlineFlags::NONE).unwrap());
        let mut deadline_entity = SchedulingEntity::new(deadline, 1, 0);
        deadline_entity.activate_deadline(0);
        queue
            .enqueue_test(
                ThreadId::from_parts(0, 1),
                deadline,
                deadline_entity,
                0,
                EnqueueReason::Wake,
            )
            .unwrap();
        queue
            .enqueue_test(
                ThreadId::from_parts(1, 1),
                stopper,
                SchedulingEntity::new(stopper, 1, 0),
                0,
                EnqueueReason::Wake,
            )
            .unwrap();

        assert_eq!(
            pick_next(&mut queue, RtEligibility::Throttled).id(),
            ThreadId::from_parts(1, 1),
            "stopper work must bypass ordinary RT bandwidth throttling"
        );
    }

    #[test]
    fn kernel_stopper_does_not_enter_the_realtime_priority_array() {
        let mut queue = RunQueue::new();
        let stopper = SchedulePolicy::kernel_stop();
        queue
            .enqueue_test(
                ThreadId::from_parts(1, 1),
                stopper,
                SchedulingEntity::new(stopper, 1, 0),
                0,
                EnqueueReason::Wake,
            )
            .unwrap();

        assert_eq!(queue.rt.count_at_priority(100), 0);
        assert_eq!(queue.placement_demand(), 0);
    }

    #[test]
    fn kernel_stopper_preempts_all_user_sched_classes() {
        let stopper = SchedulePolicy::kernel_stop();
        let stopper_entity = SchedulingEntity::new(stopper, 1, 0);
        for policy in [
            SchedulePolicy::default(),
            SchedulePolicy::fifo(RtPriority::new(99).unwrap()),
            SchedulePolicy::deadline(DeadlinePolicy::new(1, 2, 3, DeadlineFlags::NONE).unwrap()),
        ] {
            let mut entity = SchedulingEntity::new(policy, 1, 0);
            entity.activate_deadline(0);
            let current = CurrentSchedule::test_state(policy, entity);
            assert!(current.should_preempt(stopper, stopper_entity.clone(), 0));
        }

        let current = CurrentSchedule::test_state(stopper, stopper_entity);
        assert!(!current.should_preempt(
            SchedulePolicy::default(),
            SchedulingEntity::new(SchedulePolicy::default(), 1, 0),
            0
        ));
    }

    #[test]
    fn fifo_preemption_preserves_the_head_position() {
        let mut queue = RunQueue::new();
        let policy = SchedulePolicy::fifo(RtPriority::new(10).unwrap());
        for slot in [1, 2] {
            queue
                .enqueue_test(
                    ThreadId::from_parts(slot, 1),
                    policy,
                    SchedulingEntity::new(policy, 1, 0),
                    0,
                    EnqueueReason::Wake,
                )
                .unwrap();
        }
        queue
            .enqueue_test(
                ThreadId::from_parts(0, 1),
                policy,
                SchedulingEntity::new(policy, 1, 0),
                0,
                EnqueueReason::Preempted,
            )
            .unwrap();
        assert_eq!(
            pick_next(&mut queue, RtEligibility::Runnable).id(),
            ThreadId::from_parts(0, 1)
        );
    }

    #[test]
    fn first_fair_placement_cannot_start_behind_runqueue_virtual_time() {
        let mut queue = RunQueue::new();
        queue.set_virtual_time_for_test(10_000);
        let policy = SchedulePolicy::fair(Nice::ZERO, FairMode::Normal);
        let thread = ThreadId::from_parts(0, 1);

        queue
            .enqueue_test(
                thread,
                policy,
                SchedulingEntity::new(policy, 1_000, 0),
                0,
                EnqueueReason::Wake,
            )
            .unwrap();

        let entity = queue.dequeue(thread).unwrap().entity().fair().unwrap();
        assert_eq!(entity.vruntime(), 10_000);
        assert_eq!(entity.virtual_deadline(), 10_500);
    }

    #[test]
    fn fair_preemption_preserves_positive_lag_and_active_deadline() {
        let mut queue = RunQueue::new();
        queue.set_virtual_time_for_test(1_000);
        let policy = SchedulePolicy::fair(Nice::ZERO, FairMode::Normal);
        let thread = ThreadId::from_parts(0, 1);

        queue
            .enqueue_test(
                thread,
                policy,
                SchedulingEntity::Fair(FairEntity::test_state(
                    Nice::ZERO,
                    FairMode::Normal,
                    900,
                    950,
                )),
                0,
                EnqueueReason::Preempted,
            )
            .unwrap();

        let entity = queue.dequeue(thread).unwrap().entity().fair().unwrap();
        assert_eq!(
            (entity.vruntime(), entity.virtual_deadline()),
            (900, 950),
            "a same-rq preemption must not erase the current EEVDF request's lag"
        );
    }

    #[test]
    fn fair_migration_preserves_positive_lag_and_active_deadline() {
        let policy = SchedulePolicy::fair(Nice::ZERO, FairMode::Normal);
        let migrating = ThreadId::from_parts(0, 1);
        let peer = ThreadId::from_parts(1, 1);
        let mut source = RunQueue::new();
        source
            .enqueue_test(
                migrating,
                policy,
                SchedulingEntity::Fair(FairEntity::test_state(
                    Nice::ZERO,
                    FairMode::Normal,
                    900,
                    950,
                )),
                0,
                EnqueueReason::Preempted,
            )
            .unwrap();
        source
            .enqueue_test(
                peer,
                policy,
                SchedulingEntity::Fair(FairEntity::test_state(
                    Nice::ZERO,
                    FairMode::Normal,
                    1_100,
                    1_200,
                )),
                0,
                EnqueueReason::Preempted,
            )
            .unwrap();
        let detached = source
            .detach_for_transfer(migrating, None, 500_000)
            .unwrap();

        let mut destination = RunQueue::new();
        destination.set_virtual_time_for_test(2_000);
        destination
            .enqueue_test(
                peer,
                policy,
                SchedulingEntity::Fair(FairEntity::test_state(
                    Nice::ZERO,
                    FairMode::Normal,
                    2_000,
                    2_100,
                )),
                0,
                EnqueueReason::Preempted,
            )
            .unwrap();
        destination
            .enqueue_task(detached, EnqueueReason::Migrated, None)
            .unwrap();

        let entity = destination
            .dequeue(migrating)
            .unwrap()
            .entity()
            .fair()
            .unwrap();
        assert_eq!(
            (entity.vruntime(), entity.virtual_deadline()),
            (1_800, 1_850),
            "migration must restore source vlag and relative deadline on the destination rq"
        );
    }

    #[test]
    fn fair_yield_forfeits_request_before_positive_lag_peer() {
        let mut queue = RunQueue::new();
        let policy = SchedulePolicy::fair(Nice::ZERO, FairMode::Normal);
        let yielding = ThreadId::from_parts(0, 1);
        let waiting = ThreadId::from_parts(1, 1);

        queue
            .enqueue_test(
                waiting,
                policy,
                SchedulingEntity::new(policy, 100, 100),
                0,
                EnqueueReason::Migrated,
            )
            .unwrap();
        queue
            .enqueue_test(
                yielding,
                policy,
                SchedulingEntity::new(policy, 100, 0),
                0,
                EnqueueReason::Yield,
            )
            .unwrap();

        assert_eq!(
            pick_next(&mut queue, RtEligibility::Runnable).id(),
            waiting,
            "yield must forfeit the active request so positive-lag peers become eligible",
        );
    }

    #[test]
    fn weighted_virtual_time_makes_every_non_negative_lag_entity_eligible() {
        let mut queue = RunQueue::new();
        let low_weight = SchedulePolicy::fair(Nice::new(19).unwrap(), FairMode::Normal);
        let normal_weight = SchedulePolicy::fair(Nice::ZERO, FairMode::Normal);
        for (slot, policy, vruntime, deadline) in [
            (0, low_weight, 0, 100),
            (1, normal_weight, 4, 8),
            (2, normal_weight, 10, 20),
        ] {
            let SchedulePolicy::Fair { nice, mode } = policy else {
                unreachable!();
            };
            queue
                .enqueue_test(
                    ThreadId::from_parts(slot, 1),
                    policy,
                    SchedulingEntity::Fair(FairEntity::test_state(nice, mode, vruntime, deadline)),
                    0,
                    EnqueueReason::Migrated,
                )
                .unwrap();
        }

        assert_eq!(
            pick_next(&mut queue, RtEligibility::Runnable).id(),
            ThreadId::from_parts(1, 1),
            "weighted V must make both vruntime 0 and 4 eligible, then choose vd=8",
        );
    }

    #[test]
    fn fair_deadline_order_survives_virtual_time_wrap() {
        let mut queue = RunQueue::new();
        let policy = SchedulePolicy::fair(Nice::ZERO, FairMode::Normal);
        let virtual_time = u64::MAX - 100;
        queue.set_virtual_time_for_test(virtual_time);
        let later = ThreadId::from_parts(0, 1);
        let earlier = ThreadId::from_parts(1, 1);

        queue
            .enqueue_test(
                later,
                policy,
                SchedulingEntity::new(policy, 200, virtual_time),
                0,
                EnqueueReason::Migrated,
            )
            .unwrap();
        queue
            .enqueue_test(
                earlier,
                policy,
                SchedulingEntity::new(policy, 110, virtual_time),
                0,
                EnqueueReason::Migrated,
            )
            .unwrap();

        assert_eq!(
            pick_next(&mut queue, RtEligibility::Runnable).id(),
            earlier,
            "EEVDF must order wrapped virtual deadlines by signed distance",
        );
    }

    #[test]
    fn fair_weighted_virtual_time_includes_current_across_wrap() {
        let mut queue = RunQueue::new();
        let policy = SchedulePolicy::fair(Nice::ZERO, FairMode::Normal);
        let before_wrap = u64::MAX - 100;
        queue.set_virtual_time_for_test(before_wrap);
        queue
            .enqueue_test(
                ThreadId::from_parts(0, 1),
                policy,
                SchedulingEntity::Fair(FairEntity::test_state(
                    Nice::ZERO,
                    FairMode::Normal,
                    before_wrap,
                    before_wrap.wrapping_add(100),
                )),
                0,
                EnqueueReason::Migrated,
            )
            .unwrap();

        let current =
            FairEntity::test_state(Nice::ZERO, FairMode::Normal, 20, 20_u64.wrapping_add(100));
        queue.update_fair_virtual_time(Some(current));

        assert_eq!(
            queue.virtual_time(),
            u64::MAX - 40,
            "the owner-rq mean must use signed deltas and include the running entity",
        );
        queue.fair.assert_invariants();
    }

    #[test]
    fn fair_pushable_summary_uses_wrapped_runqueue_order() {
        let mut queue = RunQueue::new();
        let policy = SchedulePolicy::fair(Nice::ZERO, FairMode::Normal);
        let virtual_time = u64::MAX - 10;
        queue.set_virtual_time_for_test(virtual_time);
        for (slot, deadline) in [(0, 5), (1, u64::MAX - 1)] {
            queue
                .enqueue_test(
                    ThreadId::from_parts(slot, 1),
                    policy,
                    SchedulingEntity::Fair(FairEntity::test_state(
                        Nice::ZERO,
                        FairMode::Normal,
                        virtual_time,
                        deadline,
                    )),
                    0,
                    EnqueueReason::Migrated,
                )
                .unwrap();
        }

        assert!(queue.has_pushable_fair());
        let epoch = queue.begin_balance_scan();
        assert_eq!(
            queue
                .next_balance_candidate(epoch, |_| true)
                .expect("one Fair candidate must be movable")
                .id,
            ThreadId::from_parts(1, 1),
            "the Fair class must retain the owner runqueue's modular EEVDF order",
        );
    }

    #[test]
    fn deadline_preemption_does_not_reapply_the_cbs_wake_rule() {
        let mut queue = RunQueue::new();
        let policy =
            SchedulePolicy::deadline(DeadlinePolicy::new(4, 8, 10, DeadlineFlags::NONE).unwrap());
        let thread = ThreadId::from_parts(0, 1);
        let mut entity = SchedulingEntity::new(policy, 1, 0);
        entity.activate_deadline(0);
        assert!(!entity.charge(1, 0, 0));

        queue
            .enqueue_test(thread, policy, entity, 4, EnqueueReason::Preempted)
            .unwrap();

        let entity = queue.dequeue(thread).unwrap().entity();
        let deadline = entity.deadline().unwrap();
        assert_eq!(deadline.absolute_deadline_ns(), Some(8));
        assert_eq!(deadline.remaining_runtime_ns(), 3);
    }

    #[test]
    fn pushable_membership_tracks_each_non_idle_scheduler_class() {
        let mut queue = RunQueue::new();
        let idle = SchedulePolicy::fair(Nice::ZERO, FairMode::Idle);
        let fair = SchedulePolicy::fair(Nice::ZERO, FairMode::Normal);
        let rt = SchedulePolicy::fifo(RtPriority::new(80).unwrap());
        let deadline =
            SchedulePolicy::deadline(DeadlinePolicy::new(1, 2, 3, DeadlineFlags::NONE).unwrap());
        let idle_id = ThreadId::from_parts(0, 1);
        let fair_id = ThreadId::from_parts(1, 1);
        let rt_id = ThreadId::from_parts(2, 1);
        let deadline_id = ThreadId::from_parts(3, 1);

        queue
            .enqueue_test(
                idle_id,
                idle,
                SchedulingEntity::new(idle, 1, 0),
                0,
                EnqueueReason::Wake,
            )
            .unwrap();
        assert!(!queue.has_pushable_deadline());
        assert!(!queue.has_pushable_realtime());
        assert!(!queue.has_pushable_fair());
        for (id, policy) in [(fair_id, fair), (rt_id, rt), (deadline_id, deadline)] {
            let mut entity = SchedulingEntity::new(policy, 1, 0);
            if matches!(policy, SchedulePolicy::Deadline(_)) {
                entity.activate_deadline(0);
            }
            queue
                .enqueue_test(id, policy, entity, 0, EnqueueReason::Wake)
                .unwrap();
        }
        assert!(queue.has_pushable_deadline());
        assert!(queue.has_pushable_realtime());
        assert!(queue.has_pushable_fair());

        queue.dequeue(deadline_id).unwrap();
        assert!(!queue.has_pushable_deadline());
        assert!(queue.has_pushable_realtime());
        assert_eq!(pick_next(&mut queue, RtEligibility::Runnable).id(), rt_id);
        assert!(!queue.has_pushable_realtime());
        assert!(queue.has_pushable_fair());
        queue.dequeue(fair_id).unwrap();
        assert!(!queue.has_pushable_fair());
        assert_eq!(queue.dequeue(idle_id).unwrap().id, idle_id);
    }

    #[test]
    fn fair_virtual_time_and_pick_do_not_scan_the_runnable_set() {
        let mut queue = RunQueue::new();
        let policy = SchedulePolicy::fair(Nice::ZERO, FairMode::Normal);
        for slot in 0..128 {
            queue
                .enqueue_test(
                    ThreadId::from_parts(slot, 1),
                    policy,
                    SchedulingEntity::new(policy, 1_000, slot as u64),
                    0,
                    EnqueueReason::Migrated,
                )
                .unwrap();
        }

        reset_fair_runqueue_visits();
        queue.update_fair_virtual_time(None);
        assert_eq!(
            fair_runqueue_visits(),
            0,
            "weighted virtual time must come from incrementally maintained rq sums"
        );

        queue.fair.assert_invariants();
        while queue.has_fair() {
            reset_fair_runqueue_visits();
            pick_next(&mut queue, RtEligibility::Runnable);
            assert!(
                fair_runqueue_visits() <= 32,
                "EEVDF selection must remain logarithmic, observed {} visits",
                fair_runqueue_visits()
            );
            queue.fair.assert_invariants();
        }

        let mut removal_queue = RunQueue::new();
        for slot in 0..128 {
            removal_queue
                .enqueue_test(
                    ThreadId::from_parts(slot, 1),
                    policy,
                    SchedulingEntity::new(policy, 1_000, slot as u64),
                    0,
                    EnqueueReason::Migrated,
                )
                .unwrap();
        }
        for index in 0..128 {
            let slot = (index * 73) % 128;
            removal_queue
                .dequeue(ThreadId::from_parts(slot, 1))
                .unwrap();
            removal_queue.fair.assert_invariants();
        }
    }

    #[test]
    fn fair_enqueue_uses_direct_runqueue_membership() {
        let mut queue = RunQueue::new();
        let policy = SchedulePolicy::fair(Nice::ZERO, FairMode::Normal);
        queue
            .enqueue_test(
                ThreadId::from_parts(0, 1),
                policy,
                SchedulingEntity::new(policy, 1_000, 0),
                0,
                EnqueueReason::Wake,
            )
            .unwrap();

        reset_runqueue_membership_lookups();
        queue
            .enqueue_test(
                ThreadId::from_parts(1, 1),
                policy,
                SchedulingEntity::new(policy, 1_000, 0),
                0,
                EnqueueReason::Wake,
            )
            .unwrap();
        assert_eq!(
            runqueue_membership_lookups(),
            1,
            "enqueue must perform one generation-checked lookup instead of probing scheduler \
             classes"
        );
    }

    #[test]
    fn direct_membership_rejects_a_retired_thread_generation() {
        let mut queue = RunQueue::new();
        let policy = SchedulePolicy::fair(Nice::ZERO, FairMode::Normal);
        let retired = ThreadId::from_parts(7, 1);
        let replacement = ThreadId::from_parts(7, 2);

        queue
            .enqueue_test(
                retired,
                policy,
                SchedulingEntity::new(policy, 1_000, 0),
                0,
                EnqueueReason::Wake,
            )
            .unwrap();
        assert_eq!(queue.dequeue(retired).unwrap().id, retired);
        queue
            .enqueue_test(
                replacement,
                policy,
                SchedulingEntity::new(policy, 1_000, 0),
                0,
                EnqueueReason::Wake,
            )
            .unwrap();

        assert!(queue.dequeue(retired).is_none());
        assert_eq!(queue.dequeue(replacement).unwrap().id, replacement);
    }

    #[test]
    fn realtime_bitmap_tracks_the_highest_nonempty_priority() {
        let mut queue = RunQueue::new();
        let low = SchedulePolicy::fifo(RtPriority::new(1).unwrap());
        let high = SchedulePolicy::fifo(RtPriority::new(99).unwrap());
        let low_id = ThreadId::from_parts(0, 1);
        let high_id = ThreadId::from_parts(1, 1);
        for (id, policy) in [(low_id, low), (high_id, high)] {
            queue
                .enqueue_test(
                    id,
                    policy,
                    SchedulingEntity::new(policy, 1_000, 0),
                    0,
                    EnqueueReason::Wake,
                )
                .unwrap();
        }

        assert_eq!(queue.highest_rt_priority(), Some(99));
        assert_eq!(queue.dequeue(high_id).unwrap().id, high_id);
        assert_eq!(queue.highest_rt_priority(), Some(1));
        assert_eq!(pick_next(&mut queue, RtEligibility::Runnable).id(), low_id);
        assert!(
            queue.has_rt(),
            "selected RT current remains represented in the active bitmap"
        );
        assert_eq!(queue.dequeue(low_id).unwrap().id, low_id);
        assert!(!queue.has_rt());
    }

    #[test]
    fn realtime_running_entity_remains_linked_in_the_active_array() {
        let mut queue = RunQueue::new();
        let policy = SchedulePolicy::fifo(RtPriority::new(10).unwrap());
        let running = ThreadId::from_parts(0, 1);
        queue
            .enqueue_test(
                running,
                policy,
                SchedulingEntity::new(policy, 1_000, 0),
                0,
                EnqueueReason::Wake,
            )
            .unwrap();

        assert_eq!(pick_linked_current(&mut queue), running);
        assert!(
            queue.contains(running),
            "Linux RT keeps current in the active priority array"
        );
        assert_eq!(queue.len(), 0, "current is not a queued balance candidate");
        assert!(!queue.has_pushable_realtime());
    }

    #[test]
    fn deadline_running_entity_remains_linked_in_the_active_tree() {
        let mut queue = RunQueue::new();
        let policy =
            SchedulePolicy::deadline(DeadlinePolicy::new(10, 20, 30, DeadlineFlags::NONE).unwrap());
        let running = ThreadId::from_parts(0, 1);
        let mut entity = SchedulingEntity::new(policy, 1, 0);
        entity.activate_deadline(0);
        queue
            .enqueue_test(running, policy, entity, 0, EnqueueReason::Wake)
            .unwrap();

        assert_eq!(pick_linked_current(&mut queue), running);
        assert!(
            queue.contains(running),
            "Linux Deadline keeps current in the active EDF tree"
        );
        assert_eq!(queue.len(), 0, "current is not a queued balance candidate");
        assert!(!queue.has_pushable_deadline());
    }

    #[test]
    fn rt_class_throttle_is_all_or_nothing() {
        let mut queue = RunQueue::new();
        let ordinary = ThreadId::from_parts(0, 1);
        let lower_pi_owner = ThreadId::from_parts(1, 1);
        let higher_pi_owner = ThreadId::from_parts(2, 1);
        queue
            .enqueue_rt_test(
                ordinary,
                SchedulePolicy::fifo(RtPriority::new(99).unwrap()),
                false,
            )
            .unwrap();
        queue
            .enqueue_rt_test(
                lower_pi_owner,
                SchedulePolicy::fifo(RtPriority::new(10).unwrap()),
                true,
            )
            .unwrap();
        queue
            .enqueue_rt_test(
                higher_pi_owner,
                SchedulePolicy::fifo(RtPriority::new(20).unwrap()),
                true,
            )
            .unwrap();

        assert!(
            queue.pick_next_task(RtEligibility::Throttled).is_none(),
            "Linux skips a throttled RT class only when no boosted entity keeps the rq runnable"
        );
        assert_eq!(
            pick_next(&mut queue, RtEligibility::Runnable).id(),
            ordinary,
            "one boosted entity makes the whole RT rq runnable at normal priority order"
        );
    }

    #[test]
    fn deadline_pick_does_not_scan_the_runnable_set() {
        let mut queue = RunQueue::new();
        let policy =
            SchedulePolicy::deadline(DeadlinePolicy::new(10, 20, 30, DeadlineFlags::NONE).unwrap());
        for slot in 0..128 {
            let mut entity = SchedulingEntity::new(policy, 1, 0);
            entity.activate_deadline(slot as u64);
            queue
                .enqueue_test(
                    ThreadId::from_parts(slot, 1),
                    policy,
                    entity,
                    slot as u64,
                    EnqueueReason::Wake,
                )
                .unwrap();
        }

        reset_deadline_runqueue_visits();
        pick_next(&mut queue, RtEligibility::Runnable);
        queue.deadline.assert_invariants();
        assert!(
            deadline_runqueue_visits() <= 32,
            "EDF selection must remain logarithmic, observed {} visits",
            deadline_runqueue_visits(),
        );
    }
}
