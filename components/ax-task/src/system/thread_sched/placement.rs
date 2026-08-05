//! Linux-style scheduler placement facts.

use crate::{CpuId, CpuSet, runtime::task_runtime};

/// Linux-compatible runqueue ownership state.
///
/// `Queued` means `TASK_ON_RQ_QUEUED`: the task is runnable and owned by the
/// runqueue named by `task_cpu`, even while it is the current task. Whether a
/// scheduling-class node remains linked is owned by that class (Fair removes
/// current; RT and Deadline retain it). `Migrating` is the transient
/// `TASK_ON_RQ_MIGRATING` handoff between two runqueues.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TaskOnRunQueue {
    None,
    Queued,
    Migrating,
}

/// CPU eligibility, runqueue ownership, and execution ownership for one task.
///
/// These facts intentionally remain orthogonal, matching Linux `task_cpu()`,
/// `p->on_rq`, and `p->on_cpu`. In particular, switch-out may change `on_rq`
/// while `on_cpu` continues to name the old CPU until switch tail publishes
/// the release.
#[derive(Debug)]
pub(in crate::system) struct ThreadPlacementState {
    pub(in crate::system) affinity: CpuSet,
    pub(in crate::system) affinity_generation: u64,
    task_cpu: Option<CpuId>,
    on_rq: TaskOnRunQueue,
    on_cpu: Option<CpuId>,
    migration_request: Option<CpuId>,
}

impl ThreadPlacementState {
    pub(super) const fn new(affinity: CpuSet) -> Self {
        Self {
            affinity,
            affinity_generation: 1,
            task_cpu: None,
            on_rq: TaskOnRunQueue::None,
            on_cpu: None,
            migration_request: None,
        }
    }

    /// Returns the CPU whose runqueue owns this runnable task.
    ///
    /// Like Linux `task_on_rq_queued()`, this remains `Some` for the current
    /// task; callers that need a pushable candidate must also require
    /// `on_cpu() == None`.
    pub(in crate::system) const fn queued_cpu(&self) -> Option<CpuId> {
        match self.on_rq {
            TaskOnRunQueue::Queued => self.task_cpu,
            TaskOnRunQueue::None | TaskOnRunQueue::Migrating => None,
        }
    }

    /// Returns the CPU with the live execution claim.
    pub(in crate::system) fn execution_cpu(&self) -> Option<CpuId> {
        match (self.on_rq, self.task_cpu, self.on_cpu) {
            (TaskOnRunQueue::Queued, Some(task_cpu), Some(on_cpu)) if task_cpu == on_cpu => {
                Some(on_cpu)
            }
            _ => None,
        }
    }

    pub(in crate::system) const fn on_cpu(&self) -> Option<CpuId> {
        self.on_cpu
    }

    #[cfg(test)]
    pub(in crate::system) const fn task_cpu(&self) -> Option<CpuId> {
        self.task_cpu
    }

    /// Returns the immutable destination of a committed rq-to-rq handoff.
    pub(in crate::system) const fn committed_migration_target(&self) -> Option<CpuId> {
        match self.on_rq {
            TaskOnRunQueue::Migrating => self.task_cpu,
            TaskOnRunQueue::None | TaskOnRunQueue::Queued => None,
        }
    }

    /// Returns whether either a committed handoff or a newer request remains.
    pub(in crate::system) const fn has_pending_migration(&self) -> bool {
        self.committed_migration_target().is_some() || self.migration_request.is_some()
    }

    pub(in crate::system) fn can_continue_running_on(&self, cpu: CpuId) -> bool {
        self.execution_cpu() == Some(cpu)
            && self.migration_request.is_none()
            && self.affinity.contains(cpu)
    }

    /// Linux `task_cpu()`: the last committed runqueue assignment.
    pub(in crate::system) const fn assigned_cpu(&self) -> Option<CpuId> {
        self.task_cpu
    }

    /// Linux `activate_task()`: publish runnable ownership on one runqueue.
    pub(in crate::system) fn activate(&mut self, cpu: CpuId) {
        let valid = self.on_cpu.is_none()
            && match self.on_rq {
                TaskOnRunQueue::None => true,
                TaskOnRunQueue::Migrating => self.task_cpu == Some(cpu),
                TaskOnRunQueue::Queued => false,
            };
        placement_invariant(valid, 0x504c_0001, cpu.as_u32() as usize);
        self.task_cpu = Some(cpu);
        self.on_rq = TaskOnRunQueue::Queued;
        if self.migration_request == Some(cpu) {
            self.migration_request = None;
        }
    }

    /// Linux `block_task()`: remove a non-running runnable task from its rq.
    pub(in crate::system) fn deactivate(&mut self, cpu: CpuId) {
        let valid = self.on_rq == TaskOnRunQueue::Queued
            && self.task_cpu == Some(cpu)
            && self.on_cpu.is_none();
        placement_invariant(valid, 0x504c_0002, cpu.as_u32() as usize);
        self.on_rq = TaskOnRunQueue::None;
        self.migration_request = None;
    }

    /// Reserves the fixed destination of an off-rq remote wake publication.
    pub(in crate::system) fn begin_remote_wakeup(&mut self, target: CpuId) {
        let valid = self.on_rq == TaskOnRunQueue::None && self.on_cpu.is_none();
        placement_invariant(valid, 0x504c_0003, target.as_u32() as usize);
        self.task_cpu = Some(target);
        self.on_rq = TaskOnRunQueue::Migrating;
        self.migration_request = None;
    }

    /// Records the newest affinity request without retargeting committed work.
    pub(in crate::system) fn request_migration(&mut self, target: Option<CpuId>) {
        match self.on_rq {
            TaskOnRunQueue::Queued | TaskOnRunQueue::Migrating => {
                self.migration_request = target.filter(|target| Some(*target) != self.task_cpu);
            }
            TaskOnRunQueue::None => {
                placement_invariant(
                    self.on_cpu.is_none(),
                    0x504c_0004,
                    target.map_or(usize::MAX, |cpu| cpu.as_u32() as usize),
                );
                // Linux leaves task_cpu() unchanged for blocked tasks. The next
                // wakeup selects from the current affinity mask.
                self.migration_request = None;
            }
        }
    }

    pub(in crate::system) fn requested_migration(&self) -> Option<CpuId> {
        self.migration_request
    }

    /// Linux `put_prev_task()`: runnable and execution ownership stay intact.
    pub(in crate::system) fn put_prev(&self, cpu: CpuId) {
        let valid = self.on_rq == TaskOnRunQueue::Queued
            && self.task_cpu == Some(cpu)
            && self.on_cpu == Some(cpu)
            && self.migration_request.is_none();
        placement_invariant(valid, 0x504c_0005, cpu.as_u32() as usize);
    }

    /// Commits `TASK_ON_RQ_MIGRATING` and the new `task_cpu()` together.
    pub(in crate::system) fn begin_migration(&mut self, source: CpuId, target: CpuId) {
        let valid = source != target
            && self.on_rq == TaskOnRunQueue::Queued
            && self.task_cpu == Some(source)
            && self.on_cpu.is_none_or(|owner| owner == source);
        placement_invariant(valid, 0x504c_0006, source.as_u32() as usize);
        self.task_cpu = Some(target);
        self.on_rq = TaskOnRunQueue::Migrating;
        self.migration_request = None;
    }

    /// Removes current from the runqueue while switch tail retains `on_cpu`.
    pub(in crate::system) fn block_current(&mut self, cpu: CpuId) {
        let valid = self.on_rq == TaskOnRunQueue::Queued
            && self.task_cpu == Some(cpu)
            && self.on_cpu == Some(cpu);
        placement_invariant(valid, 0x504c_0007, cpu.as_u32() as usize);
        self.on_rq = TaskOnRunQueue::None;
        self.migration_request = None;
    }

    /// Linux `set_next_task()`: publish execution for the selected task.
    pub(in crate::system) fn set_next_task(&mut self, cpu: CpuId) {
        let valid = self.on_rq == TaskOnRunQueue::Queued
            && self.task_cpu == Some(cpu)
            && self.on_cpu.is_none_or(|owner| owner == cpu);
        placement_invariant(valid, 0x504c_0008, cpu.as_u32() as usize);
        self.on_cpu = Some(cpu);
    }

    /// Linux `finish_task()`: switch tail releases the old execution claim.
    pub(in crate::system) fn finish_task(&mut self, cpu: CpuId) {
        placement_invariant(self.on_cpu == Some(cpu), 0x504c_0009, cpu.as_u32() as usize);
        self.on_cpu = None;
    }

    /// Cancels an unconsumed remote handoff when an external owner exits a task.
    pub(in crate::system) fn cancel_remote_handoff_for_exit(&mut self) {
        let valid = self.on_cpu.is_none() && self.on_rq != TaskOnRunQueue::Queued;
        placement_invariant(
            valid,
            0x504c_000a,
            self.task_cpu
                .map_or(usize::MAX, |cpu| cpu.as_u32() as usize),
        );
        self.on_rq = TaskOnRunQueue::None;
        self.migration_request = None;
    }

    #[cfg(test)]
    pub(in crate::system) fn inject_missing_on_cpu(&mut self) {
        self.on_cpu = None;
    }

    #[cfg(test)]
    pub(in crate::system) fn inject_exiting_on_cpu(&mut self, cpu: CpuId) {
        self.task_cpu = Some(cpu);
        self.on_rq = TaskOnRunQueue::None;
        self.on_cpu = Some(cpu);
        self.migration_request = None;
    }
}

fn placement_invariant(valid: bool, code: u32, detail: usize) {
    if !valid {
        task_runtime::fatal_invariant(code, detail);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CPU0: CpuId = CpuId::new(0);
    const CPU1: CpuId = CpuId::new(1);

    fn running_placement() -> ThreadPlacementState {
        let mut placement = ThreadPlacementState::new(CpuSet::all(2));
        placement.activate(CPU0);
        placement.set_next_task(CPU0);
        placement
    }

    #[test]
    fn current_remains_on_rq_across_put_prev() {
        let mut placement = running_placement();
        assert_eq!(placement.queued_cpu(), Some(CPU0));
        assert_eq!(placement.execution_cpu(), Some(CPU0));

        placement.put_prev(CPU0);
        assert_eq!(placement.queued_cpu(), Some(CPU0));
        assert_eq!(placement.on_cpu(), Some(CPU0));

        placement.finish_task(CPU0);
        assert_eq!(placement.queued_cpu(), Some(CPU0));
        assert_eq!(placement.execution_cpu(), None);
    }

    #[test]
    fn migration_changes_on_rq_before_switch_tail_releases_on_cpu() {
        let mut placement = running_placement();
        placement.begin_migration(CPU0, CPU1);

        assert_eq!(placement.queued_cpu(), None);
        assert_eq!(placement.task_cpu(), Some(CPU1));
        assert_eq!(placement.committed_migration_target(), Some(CPU1));
        assert_eq!(placement.on_cpu(), Some(CPU0));

        placement.finish_task(CPU0);
        assert_eq!(placement.on_cpu(), None);
    }

    #[test]
    fn later_affinity_request_does_not_retarget_committed_migration() {
        let mut placement = ThreadPlacementState::new(CpuSet::all(2));
        placement.begin_remote_wakeup(CPU1);

        placement.request_migration(Some(CPU0));

        assert_eq!(placement.committed_migration_target(), Some(CPU1));
        assert_eq!(placement.requested_migration(), Some(CPU0));
        assert_eq!(placement.assigned_cpu(), Some(CPU1));

        placement.activate(CPU1);
        placement.begin_migration(CPU1, CPU0);
        assert_eq!(placement.committed_migration_target(), Some(CPU0));
        assert_eq!(placement.requested_migration(), None);
    }

    #[test]
    fn selecting_a_runnable_task_changes_only_on_cpu() {
        let mut placement = ThreadPlacementState::new(CpuSet::all(2));
        placement.activate(CPU0);
        placement.set_next_task(CPU0);

        assert_eq!(placement.queued_cpu(), Some(CPU0));
        assert_eq!(placement.execution_cpu(), Some(CPU0));
        assert_eq!(placement.on_cpu(), Some(CPU0));
    }

    #[test]
    #[should_panic(expected = "scheduler invariant reported by unit test")]
    fn unrelated_cpu_cannot_claim_a_runnable_task() {
        let mut placement = ThreadPlacementState::new(CpuSet::all(2));
        placement.activate(CPU0);
        placement.set_next_task(CPU1);
    }

    #[test]
    fn blocked_task_retains_task_cpu_without_runqueue_ownership() {
        let mut placement = running_placement();
        placement.block_current(CPU0);
        placement.finish_task(CPU0);

        assert_eq!(placement.task_cpu(), Some(CPU0));
        assert_eq!(placement.queued_cpu(), None);
        assert_eq!(placement.on_cpu(), None);
        assert_eq!(placement.assigned_cpu(), Some(CPU0));
    }

    #[test]
    fn exit_cancels_only_the_unconsumed_remote_handoff() {
        let mut placement = ThreadPlacementState::new(CpuSet::all(2));
        placement.begin_remote_wakeup(CPU1);
        placement.request_migration(Some(CPU0));

        placement.cancel_remote_handoff_for_exit();

        assert_eq!(placement.committed_migration_target(), None);
        assert_eq!(placement.requested_migration(), None);
        assert_eq!(placement.assigned_cpu(), Some(CPU1));
    }
}
