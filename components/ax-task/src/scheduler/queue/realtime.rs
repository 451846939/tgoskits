//! Fixed-priority FIFO/RR runqueue owned by the real-time scheduling class.

use alloc::{boxed::Box, sync::Arc};
use core::ptr::NonNull;

use super::{EnqueueReason, QueuedThread};
use crate::{SchedulingEntity, ThreadId};

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

    fn get_mut_by_position(&mut self, position: usize) -> Option<&mut QueuedThread> {
        let mut node = self.head.as_deref_mut();
        for _ in 0..position {
            node = node?.next.as_deref_mut();
        }
        node.map(RealtimeNode::thread_mut)
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
}

impl RealtimeRunQueue {
    pub(super) fn new() -> Self {
        Self {
            active: core::array::from_fn(|_| RealtimeLevel::new()),
            active_bitmap: 0,
            exempt_bitmap: 0,
            exempt_count: [0; FIXED_PRIORITY_LEVELS],
        }
    }

    pub(super) const fn has_any_rt(&self) -> bool {
        self.active_bitmap & RT_PRIORITY_BITMAP != 0
    }

    pub(super) fn highest_rt_priority(&self) -> Option<u8> {
        bitmap_highest_priority(self.active_bitmap & RT_PRIORITY_BITMAP)
    }

    pub(super) fn count_at_priority(&self, priority: u8) -> usize {
        priority
            .checked_sub(1)
            .and_then(|index| self.active.get(index as usize))
            .map_or(0, |level| level.len)
    }

    pub(super) fn enqueue(&mut self, thread: QueuedThread, reason: EnqueueReason) -> u8 {
        let priority = thread
            .policy
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
    ) -> Option<QueuedThread> {
        self.active
            .iter()
            .take(RT_PRIORITY_LEVELS)
            .rev()
            .find_map(|level| level.iter().find(|thread| predicate(thread)).cloned())
    }

    pub(super) fn select(&self, ordinary_may_run: bool) -> Option<QueuedThread> {
        let priority = if ordinary_may_run {
            self.highest_rt_priority()?
        } else {
            bitmap_highest_priority(self.exempt_bitmap)?
        };
        let index = (priority - 1) as usize;
        if ordinary_may_run {
            self.active[index].iter().next().cloned()
        } else {
            self.active[index]
                .iter()
                .find(|thread| thread.rt_quota_exempt)
                .cloned()
        }
    }

    pub(super) fn put_prev(
        &mut self,
        priority: u8,
        mut thread: QueuedThread,
        reason: EnqueueReason,
    ) -> Option<SchedulingEntity> {
        if thread.policy.rt_priority()?.get() != priority {
            return None;
        }
        let index = (priority - 1) as usize;
        let position = self.active[index].position(thread.id)?;
        let previous = self.active[index].iter().nth(position)?.clone();
        thread.sequence = previous.sequence;
        thread.balance_scan_epoch = previous.balance_scan_epoch;
        let move_to_tail = matches!(reason, EnqueueReason::Yield)
            || (matches!(reason, EnqueueReason::Preempted)
                && thread.entity.round_robin_quantum_expired());
        if move_to_tail {
            thread.entity.reset_round_robin_quantum(thread.policy);
        }
        self.update_exempt(index, previous.rt_quota_exempt, thread.rt_quota_exempt);
        let entity = thread.entity;
        if move_to_tail {
            let mut node = self.active[index].remove_at(position)?;
            node.thread = Some(thread);
            self.active[index].push_back(node);
        } else {
            *self.active[index].get_mut_by_position(position)? = thread;
        }
        Some(entity)
    }

    pub(super) fn update_linked_entity(
        &mut self,
        priority: u8,
        id: ThreadId,
        entity: SchedulingEntity,
    ) -> Option<()> {
        self.get_mut(priority, id)?.entity = entity;
        Some(())
    }

    fn update_exempt(&mut self, index: usize, previous: bool, next: bool) {
        match (previous, next) {
            (false, true) => {
                self.exempt_count[index] = self.exempt_count[index].saturating_add(1);
                self.exempt_bitmap |= 1_u128 << index;
            }
            (true, false) => {
                self.exempt_count[index] -= 1;
                if self.exempt_count[index] == 0 {
                    self.exempt_bitmap &= !(1_u128 << index);
                }
            }
            _ => {}
        }
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
