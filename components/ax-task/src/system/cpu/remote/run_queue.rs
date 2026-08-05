use core::ops::{Deref, DerefMut};

use super::*;
use crate::RtPriority;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WakePreemptionDecision {
    KeepCurrent,
    WakeeSelected,
    QueuedCandidateSelected,
}

/// Linux `dl_rq` bandwidth ledger owned by one runqueue lock.
#[derive(Debug)]
struct DeadlineRunQueueBandwidth {
    this_bw_scaled: u64,
    running_bw_scaled: u64,
    max_bw_scaled: u64,
}

impl DeadlineRunQueueBandwidth {
    const fn new(max_bw_scaled: u64) -> Self {
        Self {
            this_bw_scaled: 0,
            running_bw_scaled: 0,
            max_bw_scaled,
        }
    }

    fn add(&mut self, utilization_scaled: u64, active: bool) {
        let this_bw_scaled = self
            .this_bw_scaled
            .checked_add(utilization_scaled)
            .unwrap_or_else(|| {
                task_runtime::fatal_invariant(0x444c_1001, utilization_scaled as usize)
            });
        let running_bw_scaled = if active {
            self.running_bw_scaled
                .checked_add(utilization_scaled)
                .unwrap_or_else(|| {
                    task_runtime::fatal_invariant(0x444c_1002, utilization_scaled as usize)
                })
        } else {
            self.running_bw_scaled
        };
        if running_bw_scaled > this_bw_scaled {
            task_runtime::fatal_invariant(0x444c_1003, utilization_scaled as usize);
        }
        self.this_bw_scaled = this_bw_scaled;
        self.running_bw_scaled = running_bw_scaled;
    }

    fn remove(&mut self, utilization_scaled: u64, active: bool) {
        let this_bw_scaled = self
            .this_bw_scaled
            .checked_sub(utilization_scaled)
            .unwrap_or_else(|| {
                task_runtime::fatal_invariant(0x444c_1004, utilization_scaled as usize)
            });
        let running_bw_scaled = if active {
            self.running_bw_scaled
                .checked_sub(utilization_scaled)
                .unwrap_or_else(|| {
                    task_runtime::fatal_invariant(0x444c_1005, utilization_scaled as usize)
                })
        } else {
            self.running_bw_scaled
        };
        if running_bw_scaled > this_bw_scaled {
            task_runtime::fatal_invariant(0x444c_1006, utilization_scaled as usize);
        }
        self.this_bw_scaled = this_bw_scaled;
        self.running_bw_scaled = running_bw_scaled;
    }

    fn activate(&mut self, utilization_scaled: u64) {
        let running_bw_scaled = self
            .running_bw_scaled
            .checked_add(utilization_scaled)
            .unwrap_or_else(|| {
                task_runtime::fatal_invariant(0x444c_1007, utilization_scaled as usize)
            });
        if running_bw_scaled > self.this_bw_scaled {
            task_runtime::fatal_invariant(0x444c_1008, utilization_scaled as usize);
        }
        self.running_bw_scaled = running_bw_scaled;
    }

    fn deactivate(&mut self, utilization_scaled: u64) {
        self.running_bw_scaled = self
            .running_bw_scaled
            .checked_sub(utilization_scaled)
            .unwrap_or_else(|| {
                task_runtime::fatal_invariant(0x444c_1009, utilization_scaled as usize)
            });
    }

    const fn snapshot(&self) -> DeadlineBandwidthSnapshot {
        DeadlineBandwidthSnapshot {
            this_bw_scaled: self.this_bw_scaled,
            running_bw_scaled: self.running_bw_scaled,
            max_bw_scaled: self.max_bw_scaled,
        }
    }
}

impl WakePreemptionDecision {
    pub(crate) const fn requests_reschedule(self) -> bool {
        matches!(self, Self::WakeeSelected)
    }
}

/// Scheduler state protected by the target CPU's irqsave runqueue lock.
///
/// Mutable runtime accounting and switch-tail state remain owner-only in
/// [`CpuLocal`]. The current scheduling snapshot is committed here with
/// physical queue membership so a remote waker can evaluate preemption.
#[derive(Debug)]
pub(crate) struct CpuRunQueueState {
    owner: CpuId,
    clock: RunQueueClock,
    queue: RunQueue,
    current: Option<CurrentSchedule>,
    /// Deadline bandwidth and membership belong to the same transaction as
    /// physical runqueue membership. Remote wakeups therefore cannot expose a
    /// runnable Deadline entity before its CBS reservation is accounted.
    deadline_members: Vec<Arc<ThreadCore>>,
    deadline_bandwidth: DeadlineRunQueueBandwidth,
}

impl CpuRunQueueState {
    pub(crate) fn new(owner: CpuId, config: TaskSystemConfig) -> Self {
        Self {
            owner,
            clock: RunQueueClock::new(),
            queue: RunQueue::new(),
            current: None,
            deadline_members: Vec::with_capacity(config.thread_capacity()),
            deadline_bandwidth: DeadlineRunQueueBandwidth::new(
                u64::from(config.deadline_cap_percent()) * 10_000_000,
            ),
        }
    }

    /// Updates and snapshots Linux-style `rq->clock` under this runqueue lock.
    pub(crate) fn update_clock(&mut self) -> RunQueueClockSnapshot {
        let source = task_runtime::scheduler_clock_source(RuntimeCpuId::new(self.owner.as_u32()));
        self.clock.update(source)
    }

    /// Reads the last owner-accepted runqueue clock without sampling hardware.
    pub(crate) fn clock_snapshot(&self) -> Option<RunQueueClockSnapshot> {
        self.clock.snapshot()
    }

    pub(crate) const fn current(&self) -> Option<CurrentSchedule> {
        self.current
    }

    pub(crate) fn set_current(&mut self, current: Option<CurrentSchedule>) {
        if let Some(current) = current {
            self.queue
                .update_linked_current(
                    current.thread(),
                    current.schedule_policy(),
                    current.scheduling_entity(),
                )
                .expect("running RT/DL linkage must match the dispatch snapshot");
        }
        self.current = current;
    }

    pub(crate) fn highest_rt_priority_including_current(&self) -> Option<u8> {
        let current = self
            .current
            .and_then(|current| current.schedule_policy().rt_priority())
            .map(RtPriority::get);
        match (current, self.highest_rt_priority()) {
            (Some(current), Some(queued)) => Some(current.max(queued)),
            (Some(priority), None) | (None, Some(priority)) => Some(priority),
            (None, None) => None,
        }
    }

    pub(crate) fn earliest_deadline_including_current(&self) -> Option<u64> {
        let current = self.current.and_then(CurrentSchedule::absolute_deadline_ns);
        match (current, self.earliest_deadline_ns()) {
            (Some(current), Some(queued)) => Some(current.min(queued)),
            (Some(deadline), None) | (None, Some(deadline)) => Some(deadline),
            (None, None) => None,
        }
    }

    pub(crate) fn deadline_members_are_empty(&self) -> bool {
        self.deadline_members.is_empty()
    }

    pub(crate) fn register_deadline_member(&mut self, core: &Arc<ThreadCore>) -> bool {
        if self
            .deadline_members
            .iter()
            .any(|member| Arc::ptr_eq(member, core))
        {
            return false;
        }
        assert!(
            self.deadline_members.len() < self.deadline_members.capacity(),
            "thread construction must reserve every Deadline member slot"
        );
        self.deadline_members.push(Arc::clone(core));
        true
    }

    pub(crate) fn unregister_deadline_member(&mut self, core: &Arc<ThreadCore>) {
        if let Some(index) = self
            .deadline_members
            .iter()
            .position(|member| Arc::ptr_eq(member, core))
        {
            self.deadline_members.swap_remove(index);
        }
    }

    pub(crate) fn add_deadline_bandwidth(&mut self, utilization_scaled: u64, active: bool) {
        self.deadline_bandwidth.add(utilization_scaled, active);
    }

    pub(crate) fn remove_deadline_bandwidth(&mut self, utilization_scaled: u64, active: bool) {
        self.deadline_bandwidth.remove(utilization_scaled, active);
    }

    pub(crate) fn activate_deadline_bandwidth(&mut self, utilization_scaled: u64) {
        self.deadline_bandwidth.activate(utilization_scaled);
    }

    pub(crate) fn deactivate_deadline_bandwidth(&mut self, utilization_scaled: u64) {
        self.deadline_bandwidth.deactivate(utilization_scaled);
    }

    pub(crate) const fn deadline_bandwidth(&self) -> DeadlineBandwidthSnapshot {
        self.deadline_bandwidth.snapshot()
    }

    /// Applies Linux EEVDF wakeup preemption to the complete owner runqueue.
    ///
    /// A fair wakee may request rescheduling only when it both defeats the
    /// protected current request and is itself the earliest eligible queued
    /// entity. Comparing only the wakee with current creates needless
    /// reschedule IPIs when an older queued contender would be selected.
    pub(crate) fn wakee_preemption(
        &self,
        wakee: ThreadId,
        policy: SchedulePolicy,
        entity: SchedulingEntity,
        fair_virtual_time: u64,
    ) -> WakePreemptionDecision {
        let Some(current) = self.current else {
            return WakePreemptionDecision::WakeeSelected;
        };
        if !current.should_preempt(policy, entity, fair_virtual_time) {
            return WakePreemptionDecision::KeepCurrent;
        }
        match policy {
            SchedulePolicy::Fair { mode, .. } => {
                if self
                    .queue
                    .fair_wakee_is_selected(wakee, mode, fair_virtual_time)
                {
                    WakePreemptionDecision::WakeeSelected
                } else {
                    WakePreemptionDecision::QueuedCandidateSelected
                }
            }
            _ => WakePreemptionDecision::WakeeSelected,
        }
    }
}

impl Deref for CpuRunQueueState {
    type Target = RunQueue;

    fn deref(&self) -> &Self::Target {
        &self.queue
    }
}

impl DerefMut for CpuRunQueueState {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.queue
    }
}
