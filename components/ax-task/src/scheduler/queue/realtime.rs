//! Fixed-priority FIFO/RR runqueue owned by the real-time scheduling class.

use alloc::{boxed::Box, sync::Arc};
use core::ptr::NonNull;

use super::{EnqueueReason, QueuedThread, QueuedThreadSnapshot};
use crate::{SchedulePolicy, SchedulingEntity, ThreadId};

const RT_PRIORITY_LEVELS: usize = 99;
const FIXED_PRIORITY_LEVELS: usize = RT_PRIORITY_LEVELS;
const RT_PRIORITY_BITMAP: u128 = (1_u128 << RT_PRIORITY_LEVELS) - 1;

/// Per-thread RT linkage prepared during thread construction.
#[derive(Debug)]
pub(crate) struct RealtimeNode {
    thread: Option<QueuedThread>,
    next: Option<Box<RealtimeNode>>,
}

impl RealtimeNode {
    pub(crate) fn empty() -> Box<Self> {
        Box::new(Self {
            thread: None,
            next: None,
        })
    }

    fn reset(&mut self, thread: QueuedThread) {
        self.thread = Some(thread);
        self.next = None;
    }

    fn thread(&self) -> &QueuedThread {
        self.thread
            .as_ref()
            .expect("linked RT node must own one scheduling entity")
    }

    fn thread_mut(&mut self) -> &mut QueuedThread {
        self.thread
            .as_mut()
            .expect("linked RT node must own one scheduling entity")
    }
}

#[derive(Debug)]
struct RealtimeLevel {
    head: Option<Box<RealtimeNode>>,
    tail: Option<NonNull<RealtimeNode>>,
    len: usize,
}

// SAFETY: `tail` points into the Box chain owned by `head`. The complete level
// is moved only while its enclosing runqueue is exclusively owned.
unsafe impl Send for RealtimeLevel {}

impl RealtimeLevel {
    const fn new() -> Self {
        Self {
            head: None,
            tail: None,
            len: 0,
        }
    }

    fn push_front(&mut self, mut node: Box<RealtimeNode>) {
        if self.head.is_none() {
            self.tail = Some(NonNull::from(node.as_mut()));
        }
        node.next = self.head.take();
        self.head = Some(node);
        self.len += 1;
    }

    fn push_back(&mut self, mut node: Box<RealtimeNode>) {
        let node_pointer = NonNull::from(node.as_mut());
        match self.tail {
            Some(mut tail) => unsafe {
                // SAFETY: `tail` is the last node of the Box chain owned by
                // this level and the runqueue lock provides unique access.
                tail.as_mut().next = Some(node);
            },
            None => self.head = Some(node),
        }
        self.tail = Some(node_pointer);
        self.len += 1;
    }

    fn remove_at(&mut self, position: usize) -> Option<Box<RealtimeNode>> {
        if position >= self.len {
            return None;
        }
        let mut previous = None;
        let mut link = &mut self.head;
        for _ in 0..position {
            let node = link.as_mut()?;
            previous = Some(NonNull::from(node.as_mut()));
            link = &mut node.next;
        }
        let mut removed = link.take()?;
        *link = removed.next.take();
        self.len -= 1;
        if self.tail == Some(NonNull::from(removed.as_mut())) {
            self.tail = previous;
        }
        if self.head.is_none() {
            self.tail = None;
        }
        Some(removed)
    }

    fn position(&self, id: ThreadId) -> Option<usize> {
        self.iter().position(|thread| thread.id == id)
    }

    fn iter(&self) -> RealtimeIter<'_> {
        RealtimeIter {
            next: self.head.as_deref(),
        }
    }
}

impl Drop for RealtimeLevel {
    fn drop(&mut self) {
        while let Some(mut node) = self.head.take() {
            self.head = node.next.take();
        }
        self.tail = None;
        self.len = 0;
    }
}

struct RealtimeIter<'queue> {
    next: Option<&'queue RealtimeNode>,
}

impl<'queue> Iterator for RealtimeIter<'queue> {
    type Item = &'queue QueuedThread;

    fn next(&mut self) -> Option<Self::Item> {
        let node = self.next?;
        self.next = node.next.as_deref();
        Some(node.thread())
    }
}

/// Linux-style RT priority array: one intrusive FIFO per priority plus cached
/// bitmaps. Queue nodes are embedded scheduler storage; enqueue/dequeue never
/// allocate or free memory while the rq lock is held.
#[derive(Debug)]
pub(super) struct RealtimeRunQueue {
    active: [RealtimeLevel; FIXED_PRIORITY_LEVELS],
    active_bitmap: u128,
    exempt_bitmap: u128,
    exempt_count: [usize; FIXED_PRIORITY_LEVELS],
    pushable_bitmap: u128,
}

impl RealtimeRunQueue {
    pub(super) fn new() -> Self {
        Self {
            active: core::array::from_fn(|_| RealtimeLevel::new()),
            active_bitmap: 0,
            exempt_bitmap: 0,
            exempt_count: [0; FIXED_PRIORITY_LEVELS],
            pushable_bitmap: 0,
        }
    }

    pub(super) const fn has_any_rt(&self) -> bool {
        self.active_bitmap & RT_PRIORITY_BITMAP != 0
    }

    pub(super) const fn has_exempt_rt(&self) -> bool {
        self.exempt_bitmap & RT_PRIORITY_BITMAP != 0
    }

    pub(super) fn highest_rt_priority(&self) -> Option<u8> {
        bitmap_highest_priority(self.active_bitmap & RT_PRIORITY_BITMAP)
    }

    pub(super) const fn has_pushable(&self) -> bool {
        self.pushable_bitmap & RT_PRIORITY_BITMAP != 0
    }

    pub(super) fn refresh_pushable_priority(&mut self, priority: u8, current: Option<ThreadId>) {
        let index = (priority - 1) as usize;
        let mut pushable = false;
        for thread in self.active[index].iter() {
            if thread.migration_capable && current != Some(thread.id) {
                pushable = true;
            }
        }
        let bit = 1_u128 << index;
        if pushable {
            self.pushable_bitmap |= bit;
        } else {
            self.pushable_bitmap &= !bit;
        }
    }

    pub(super) fn count_at_priority(&self, priority: u8) -> usize {
        priority
            .checked_sub(1)
            .and_then(|index| self.active.get(index as usize))
            .map_or(0, |level| level.len)
    }

    pub(super) fn enqueue(&mut self, thread: QueuedThread, reason: EnqueueReason) -> u8 {
        let priority = thread
            .active
            .policy()
            .rt_priority()
            .expect("RT priority array requires FIFO or RR policy")
            .get();
        let index = (priority - 1) as usize;
        if thread.rt_quota_exempt {
            self.exempt_count[index] = self.exempt_count[index].saturating_add(1);
            self.exempt_bitmap |= 1_u128 << index;
        }
        let core = Arc::clone(&thread.core);
        let mut node = unsafe {
            // SAFETY: the placement state and target rq lock serialize the
            // only RT linkage belonging to this thread.
            core.runqueue_nodes().take_realtime()
        };
        node.reset(thread);
        if reason == EnqueueReason::Preempted {
            self.active[index].push_front(node);
        } else {
            self.active[index].push_back(node);
        }
        self.active_bitmap |= 1_u128 << index;
        priority
    }

    pub(super) fn remove(&mut self, priority: u8, id: ThreadId) -> Option<QueuedThread> {
        let index = (priority - 1) as usize;
        let position = self.active[index].position(id)?;
        let node = self.active[index].remove_at(position)?;
        Some(self.after_remove(index, node))
    }

    pub(super) fn get(&self, priority: u8, id: ThreadId) -> Option<&QueuedThread> {
        self.active[(priority - 1) as usize]
            .iter()
            .find(|thread| thread.id == id)
    }

    pub(super) fn get_mut(&mut self, priority: u8, id: ThreadId) -> Option<&mut QueuedThread> {
        let mut node = self.active[(priority - 1) as usize].head.as_deref_mut();
        while let Some(current) = node {
            if current.thread().id == id {
                return Some(current.thread_mut());
            }
            node = current.next.as_deref_mut();
        }
        None
    }

    pub(super) fn find_first_matching(
        &self,
        predicate: &mut impl FnMut(&QueuedThread) -> bool,
    ) -> Option<QueuedThreadSnapshot> {
        self.active
            .iter()
            .take(RT_PRIORITY_LEVELS)
            .rev()
            .find_map(|level| {
                level
                    .iter()
                    .find(|thread| predicate(thread))
                    .map(QueuedThreadSnapshot::from)
            })
    }

    pub(super) fn select(&self) -> Option<QueuedThreadSnapshot> {
        let priority = self.highest_rt_priority()?;
        let index = (priority - 1) as usize;
        self.active[index]
            .iter()
            .next()
            .map(QueuedThreadSnapshot::from)
    }

    pub(super) fn put_prev_current(
        &mut self,
        priority: u8,
        id: ThreadId,
        reason: EnqueueReason,
    ) -> Option<SchedulingEntity> {
        let index = (priority - 1) as usize;
        let position = self.active[index].position(id)?;
        let move_to_tail = matches!(reason, EnqueueReason::Yield);
        if move_to_tail {
            let node = self.active[index].remove_at(position)?;
            let entity = node.thread().active.entity().clone();
            self.active[index].push_back(node);
            Some(entity)
        } else {
            self.active[index]
                .iter()
                .nth(position)
                .map(QueuedThread::entity)
        }
    }

    /// Linux `task_tick_rt()` for one linked RR current.
    ///
    /// The current task stays in the active priority array.  Expiration
    /// refreshes its quantum unconditionally; only a peer at the same
    /// priority causes `requeue_task_rt()` and a reschedule request.
    pub(super) fn task_tick_round_robin(
        &mut self,
        priority: u8,
        id: ThreadId,
        policy: SchedulePolicy,
    ) -> Option<bool> {
        let index = (priority - 1) as usize;
        let position = self.active[index].position(id)?;
        let expired = self.active[index]
            .iter()
            .nth(position)?
            .active
            .entity()
            .round_robin_quantum_expired();
        if !expired {
            return Some(false);
        }

        let has_peer = self.active[index].len > 1;
        if has_peer {
            let mut node = self.active[index].remove_at(position)?;
            node.thread_mut()
                .active
                .entity_mut()
                .reset_round_robin_quantum(policy);
            self.active[index].push_back(node);
        } else {
            self.active[index]
                .head
                .as_deref_mut()?
                .thread_mut()
                .active
                .entity_mut()
                .reset_round_robin_quantum(policy);
        }
        Some(has_peer)
    }

    fn after_remove(&mut self, index: usize, mut node: Box<RealtimeNode>) -> QueuedThread {
        let thread = node
            .thread
            .take()
            .expect("removed RT node must retain its scheduling entity");
        if thread.rt_quota_exempt {
            self.exempt_count[index] -= 1;
            if self.exempt_count[index] == 0 {
                self.exempt_bitmap &= !(1_u128 << index);
            }
        }
        if self.active[index].len == 0 {
            self.active_bitmap &= !(1_u128 << index);
            debug_assert_eq!(self.exempt_count[index], 0);
            self.exempt_bitmap &= !(1_u128 << index);
            self.pushable_bitmap &= !(1_u128 << index);
        }
        unsafe {
            // SAFETY: the node is no longer linked and placement prevents a
            // concurrent enqueue until this rq transaction returns it.
            thread.core.runqueue_nodes().return_realtime(node);
        }
        thread
    }
}

fn bitmap_highest_priority(bitmap: u128) -> Option<u8> {
    (bitmap != 0).then(|| (u128::BITS - bitmap.leading_zeros()) as u8)
}
