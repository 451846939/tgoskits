//! Fixed-capacity owner-CPU task-deadline storage.

mod heap;
mod node;

pub use node::{
    ExpiredTaskDeadline, TaskDeadlineKind, TaskDeadlineNode, TaskDeadlineRegistration,
    TaskDeadlineToken,
};

use self::{
    heap::{TimerEntry, TimerHeap},
    node::{TASK_DEADLINE_CLASS_COUNT, TaskDeadlineNodeId},
};
use crate::runtime::{MonotonicDeadline, MonotonicInstant};

/// Failure returned while arming a fixed-capacity timer.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum TaskDeadlineError {
    /// Every preallocated heap slot is occupied by an active task deadline.
    #[error("per-CPU timer capacity is exhausted")]
    Capacity,
    /// The node identity or arm generation space has been exhausted.
    #[error("timer identity or generation space is exhausted")]
    GenerationExhausted,
    /// The typed event does not belong to the supplied embedded timer node.
    #[error("task deadline kind does not match its timer node")]
    KindMismatch,
}

/// Bounded timer-IRQ expiration request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskDeadlineExpireRequest {
    now: MonotonicInstant,
    batch_limit: usize,
}

impl TaskDeadlineExpireRequest {
    /// Creates one bounded timer expiration request.
    pub const fn new(now: MonotonicInstant, batch_limit: usize) -> Self {
        Self { now, batch_limit }
    }
}

/// Result of one bounded timer-IRQ pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskDeadlineExpireBatch {
    processed: usize,
    expired: usize,
    pending: bool,
    next_deadline: Option<MonotonicDeadline>,
}

impl TaskDeadlineExpireBatch {
    /// Returns heap nodes removed during this pass.
    pub const fn processed(self) -> usize {
        self.processed
    }

    /// Returns valid expirations written into the caller's output storage.
    pub const fn expired(self) -> usize {
        self.expired
    }

    /// Reports that immediately actionable work remains after the batch.
    pub const fn pending(self) -> bool {
        self.pending
    }

    /// Returns the next logical task deadline.
    pub const fn next_deadline(self) -> Option<MonotonicDeadline> {
        self.next_deadline
    }
}

/// Fixed-capacity value heap created during CPU-local initialization.
///
/// Construction is the only operation that reserves memory. Arming, cancelling,
/// and expiring never grow or shrink the allocation.
#[derive(Debug)]
pub struct TaskDeadlineQueue {
    heap: TimerHeap,
    capacity_per_class: usize,
    class_counts: [usize; TASK_DEADLINE_CLASS_COUNT],
}

/// Reversible removal of one task-deadline queue entry.
///
/// The owner CPU keeps this transaction while it derives and publishes the
/// replacement clockevent state. A pre-publication failure can therefore
/// restore the exact generation-bearing entry without allocating a new slot or
/// consuming another timer generation.
#[must_use = "a task-deadline cancellation must be committed or rolled back"]
pub(crate) struct TaskDeadlineCancelTxn {
    entry: TimerEntry,
}

/// Fully validated arm operation whose queue commit cannot fail.
///
/// Owner code may prepare every timer affected by one scheduler transition
/// before changing any queue entry. This is the task-deadline equivalent of
/// Linux hrtimer's prepare/enqueue split: capacity and generation failures stay
/// on the recoverable side of the scheduler commit boundary.
#[must_use = "a prepared task deadline must be committed or discarded"]
pub(crate) struct TaskDeadlineArmPlan {
    entry: TimerEntry,
    replacing: bool,
}

impl TaskDeadlineCancelTxn {
    pub(crate) const fn commit(self) {}

    pub(crate) fn rollback(self, queue: &mut TaskDeadlineQueue) {
        queue.restore_cancelled(self.entry);
    }
}

impl TaskDeadlineQueue {
    /// Preallocates `capacity` independent slots for each typed timer class.
    pub fn new(capacity: usize) -> Self {
        let total_capacity = capacity
            .checked_mul(TASK_DEADLINE_CLASS_COUNT)
            .expect("task deadline capacity exceeds addressable storage");
        Self {
            heap: TimerHeap::new(total_capacity),
            capacity_per_class: capacity,
            class_counts: [0; TASK_DEADLINE_CLASS_COUNT],
        }
    }

    /// Arms a typed task deadline for an absolute monotonic deadline.
    ///
    /// Rearming replaces this physical node's previous entry in place. Distinct
    /// nodes for one thread remain independent, and each node consumes at most
    /// one preallocated heap slot.
    ///
    /// # Errors
    ///
    /// Returns [`TaskDeadlineError::Capacity`] without changing the queue or
    /// consuming an arm generation if no heap slot remains. A node may retain
    /// the lazily assigned identity used for this capacity check. Returns
    /// [`TaskDeadlineError::GenerationExhausted`] instead of reusing an old
    /// generation.
    ///
    /// Queue mutation must remain serialized on its owner CPU. The returned
    /// move-only registration owns the physical entry; the queue stores the
    /// thread, generation, and event kind by value and does not retain `node`.
    pub fn arm(
        &mut self,
        node: &TaskDeadlineNode,
        deadline: MonotonicDeadline,
        kind: TaskDeadlineKind,
    ) -> Result<TaskDeadlineRegistration, TaskDeadlineError> {
        let plan = self.prepare_arm(node, deadline, kind)?;
        Ok(self.commit_arm(plan))
    }

    pub(crate) fn prepare_arm(
        &self,
        node: &TaskDeadlineNode,
        deadline: MonotonicDeadline,
        kind: TaskDeadlineKind,
    ) -> Result<TaskDeadlineArmPlan, TaskDeadlineError> {
        let thread = node.thread();
        let class = kind.class();
        if node.class() != class {
            return Err(TaskDeadlineError::KindMismatch);
        }
        let identity = node.identity()?;
        let replacing = self.heap.contains_node(identity);
        if self.class_counts[class.index()] == self.capacity_per_class && !replacing {
            return Err(TaskDeadlineError::Capacity);
        }
        let token = node.next_token(identity)?;
        Ok(TaskDeadlineArmPlan {
            entry: TimerEntry::new(deadline, thread, token, kind),
            replacing,
        })
    }

    pub(crate) fn commit_arm(&mut self, plan: TaskDeadlineArmPlan) -> TaskDeadlineRegistration {
        let TaskDeadlineArmPlan { entry, replacing } = plan;
        let identity = entry.token().node();
        if replacing {
            let removed = self.heap.remove_node(identity);
            assert!(
                removed.is_some(),
                "prepared replacement must retain its physical task deadline entry"
            );
        } else {
            let class = entry.kind().class();
            assert!(
                self.class_counts[class.index()] < self.capacity_per_class,
                "prepared task deadline must retain its reserved class capacity"
            );
            self.class_counts[class.index()] += 1;
        }
        self.heap.push(entry);
        TaskDeadlineRegistration::new(
            entry.thread(),
            entry.token(),
            entry.deadline(),
            entry.kind(),
        )
    }

    /// Cancels one matching arm operation and immediately releases its heap slot.
    ///
    /// Unlike lazy tombstoning, physical removal releases capacity immediately
    /// and makes the registration terminal as soon as this method returns.
    pub fn cancel(&mut self, registration: &TaskDeadlineRegistration) -> bool {
        let Some(cancellation) = self.begin_cancel(registration) else {
            return false;
        };
        cancellation.commit();
        true
    }

    pub(crate) fn begin_cancel(
        &mut self,
        registration: &TaskDeadlineRegistration,
    ) -> Option<TaskDeadlineCancelTxn> {
        let entry = self.heap.remove(
            registration.thread(),
            registration.token(),
            registration.kind(),
        )?;
        self.class_counts[registration.kind().class().index()] -= 1;
        Some(TaskDeadlineCancelTxn { entry })
    }

    fn restore_cancelled(&mut self, entry: TimerEntry) {
        let class = entry.kind().class();
        assert!(
            !self.heap.contains_node(entry.token().node()),
            "cancelled task deadline node was reused before transaction completion"
        );
        assert!(
            self.class_counts[class.index()] < self.capacity_per_class,
            "cancelled task deadline class lost its reserved rollback capacity"
        );
        self.class_counts[class.index()] += 1;
        self.heap.push(entry);
    }

    /// Returns the earliest logical task deadline without mutating the queue.
    pub fn next_deadline(&self) -> Option<MonotonicDeadline> {
        self.heap.peek().map(TimerEntry::deadline)
    }

    pub(crate) fn has_immediately_actionable_entry(&self, now: MonotonicInstant) -> bool {
        let Some(entry) = self.heap.peek() else {
            return false;
        };
        now.reached(entry.deadline())
    }

    /// Expires timers into caller-provided storage without allocating or invoking
    /// callbacks.
    pub fn expire(
        &mut self,
        request: TaskDeadlineExpireRequest,
        output: &mut [ExpiredTaskDeadline],
    ) -> TaskDeadlineExpireBatch {
        let mut processed = 0;
        let mut expired = 0;

        while processed < request.batch_limit {
            let Some(entry) = self.heap.peek() else {
                break;
            };
            if !request.now.reached(entry.deadline()) {
                break;
            }
            if expired == output.len() {
                break;
            }

            let entry = self
                .heap
                .pop_min()
                .expect("peek proved the fixed timer heap is non-empty");
            self.class_counts[entry.kind().class().index()] -= 1;
            processed += 1;
            output[expired] = ExpiredTaskDeadline::new(
                entry.thread(),
                entry.token(),
                entry.deadline(),
                entry.kind(),
            );
            expired += 1;
        }

        let (pending, next_deadline) = self.next_wakeup(request);
        TaskDeadlineExpireBatch {
            processed,
            expired,
            pending,
            next_deadline,
        }
    }

    /// Returns the preallocated entry capacity.
    pub const fn capacity(&self) -> usize {
        self.capacity_per_class
    }

    /// Returns the number of active task deadline entries in storage.
    pub fn len(&self) -> usize {
        self.heap.len()
    }

    /// Reports whether no timer entries remain.
    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    fn next_wakeup(&self, request: TaskDeadlineExpireRequest) -> (bool, Option<MonotonicDeadline>) {
        let Some(entry) = self.heap.peek() else {
            return (false, None);
        };
        (
            request.now.reached(entry.deadline()),
            Some(entry.deadline()),
        )
    }
}

#[cfg(test)]
mod tests;
