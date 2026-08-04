//! Priority-inheritance mutex metadata.

use core::{
    cell::UnsafeCell,
    marker::PhantomPinned,
    pin::Pin,
    ptr::NonNull,
    sync::atomic::{AtomicBool, Ordering},
};

use ax_task::ThreadId;

/// A pinned waiter embedded in the blocked `RawMutex::lock` call frame.
pub(crate) struct WaiterNode {
    thread_id: ThreadId,
    granted: AtomicBool,
    prev: UnsafeCell<Option<NonNull<WaiterNode>>>,
    next: UnsafeCell<Option<NonNull<WaiterNode>>>,
    _pinned: PhantomPinned,
}

/// Unordered intrusive waiter ownership list.
#[derive(Debug)]
pub(crate) struct WaiterQueue {
    head: Option<NonNull<WaiterNode>>,
    len: usize,
}

/// Pointer removed from a [`WaiterQueue`] while the waiter remains pinned.
pub(crate) struct WaiterPointer(NonNull<WaiterNode>);

impl WaiterNode {
    /// Creates an unlinked waiter owned by the current lock call frame.
    pub(crate) const fn new(thread_id: ThreadId) -> Self {
        Self {
            thread_id,
            granted: AtomicBool::new(false),
            prev: UnsafeCell::new(None),
            next: UnsafeCell::new(None),
            _pinned: PhantomPinned,
        }
    }

    /// Returns whether unlock has transferred ownership to this waiter.
    pub(crate) fn is_granted(&self) -> bool {
        self.granted.load(Ordering::Acquire)
    }

    /// Returns the generation-bearing scheduler identity of this waiter.
    pub(crate) const fn thread_id(&self) -> ThreadId {
        self.thread_id
    }

    #[cfg(test)]
    fn new_for_test(thread_id: ThreadId) -> Pin<alloc::boxed::Box<Self>> {
        alloc::boxed::Box::pin(Self::new(thread_id))
    }

    /// Publishes ownership before the selected waiter is woken.
    fn grant(&self) {
        self.granted.store(true, Ordering::Release);
    }

    /// Reads the intrusive link while the mutex metadata lock is held.
    ///
    /// # Safety
    ///
    /// The caller must hold the metadata lock that exclusively owns the list.
    unsafe fn next(&self) -> Option<NonNull<Self>> {
        // SAFETY: required by this method's contract.
        unsafe { *self.next.get() }
    }

    /// Updates the intrusive link while the mutex metadata lock is held.
    ///
    /// # Safety
    ///
    /// The caller must hold the metadata lock that exclusively owns the list.
    unsafe fn set_next(&self, next: Option<NonNull<Self>>) {
        // SAFETY: required by this method's contract.
        unsafe { *self.next.get() = next };
    }

    unsafe fn prev(&self) -> Option<NonNull<Self>> {
        // SAFETY: required by this method's contract.
        unsafe { *self.prev.get() }
    }

    unsafe fn set_prev(&self, prev: Option<NonNull<Self>>) {
        // SAFETY: required by this method's contract.
        unsafe { *self.prev.get() = prev };
    }
}

impl WaiterQueue {
    /// Creates an empty waiter queue suitable for static mutex initialization.
    pub(crate) const fn new() -> Self {
        Self { head: None, len: 0 }
    }

    /// Inserts one pinned waiter without performing scheduler selection.
    ///
    /// # Safety
    ///
    /// `waiter` must remain pinned and alive until it is removed from this
    /// queue. The caller must hold the mutex metadata lock.
    pub(crate) unsafe fn insert(&mut self, waiter: Pin<&WaiterNode>) {
        let waiter_ptr = NonNull::from(waiter.get_ref());
        // SAFETY: the metadata lock is held and waiter is not linked yet.
        unsafe {
            waiter.set_prev(None);
            waiter.set_next(self.head);
        }
        if let Some(head) = self.head {
            // SAFETY: the old head remains pinned under the metadata lock.
            unsafe { head.as_ref().set_prev(Some(waiter_ptr)) };
        }
        self.head = Some(waiter_ptr);
        self.len += 1;
    }

    /// Removes and returns the most urgent waiter.
    ///
    /// The caller must hold the mutex metadata lock.
    #[cfg(test)]
    pub(crate) fn pop_front(&mut self) -> Option<WaiterPointer> {
        let head = self.head?;
        // SAFETY: queue pointers remain live until removal and metadata is held.
        let head_ref = unsafe { head.as_ref() };
        // SAFETY: the metadata lock is held.
        self.head = unsafe { head_ref.next() };
        if let Some(next) = self.head {
            // SAFETY: the new head remains linked under the metadata lock.
            unsafe { next.as_ref().set_prev(None) };
        }
        // SAFETY: the removed node is no longer part of this list.
        unsafe {
            head_ref.set_prev(None);
            head_ref.set_next(None);
        }
        self.len -= 1;
        Some(WaiterPointer(head))
    }

    /// Removes a previously selected waiter.
    ///
    /// The caller must hold the metadata lock across selection and removal.
    pub(crate) fn remove(&mut self, selected: &WaiterPointer) -> Option<WaiterPointer> {
        let selected_ref = unsafe {
            // SAFETY: callers may only construct a pointer from the pinned
            // waiter owned by this metadata transaction.
            selected.0.as_ref()
        };
        let (previous, next) = unsafe {
            // SAFETY: the metadata lock serializes both intrusive links.
            (selected_ref.prev(), selected_ref.next())
        };
        if previous.is_none() && self.head != Some(selected.0) {
            return None;
        }
        if let Some(previous) = previous {
            // SAFETY: previous is a live linked waiter under metadata lock.
            unsafe { previous.as_ref().set_next(next) };
        } else {
            self.head = next;
        }
        if let Some(next) = next {
            // SAFETY: next is a live linked waiter under metadata lock.
            unsafe { next.as_ref().set_prev(previous) };
        }
        // SAFETY: selected is now detached from both neighbors.
        unsafe {
            selected_ref.set_prev(None);
            selected_ref.set_next(None);
        }
        self.len -= 1;
        Some(WaiterPointer(selected.0))
    }

    /// Returns whether the queue contains no waiters.
    pub(crate) const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl WaiterPointer {
    /// Creates a metadata-owned pointer for a pinned local waiter.
    pub(crate) fn from_pin(waiter: Pin<&WaiterNode>) -> Self {
        Self(NonNull::from(waiter.get_ref()))
    }

    /// Returns the selected waiter's thread identity.
    /// # Safety
    ///
    /// The waiter must still be pinned in its lock call frame.
    #[cfg(test)]
    pub(crate) unsafe fn thread_id(&self) -> ThreadId {
        // SAFETY: forwarded caller contract keeps the waiter alive.
        unsafe { self.node() }.thread_id
    }

    /// Publishes ownership transfer to the waiter.
    ///
    /// # Safety
    ///
    /// The waiter must still be pinned in its lock call frame.
    pub(crate) unsafe fn grant(&self) {
        // SAFETY: forwarded caller contract keeps the waiter alive.
        unsafe { self.node() }.grant();
    }

    unsafe fn node(&self) -> &WaiterNode {
        // SAFETY: required by this method's caller contract.
        unsafe { self.0.as_ref() }
    }
}

// SAFETY: list pointers are accessed only while the owning mutex's SpinNoPreempt
// metadata lock is held. Nodes remain pinned until they are removed and granted.
unsafe impl Send for WaiterQueue {}

// SAFETY: `granted` is atomic; intrusive links are touched only under metadata;
// the generation-bearing thread identity is immutable while published.
unsafe impl Sync for WaiterNode {}

#[cfg(test)]
mod tests {
    use ax_task::{
        CpuId, FairMode, Nice, PiLockIdentity, PiWaitOwner, PiWaitToken, RtPriority,
        SchedulePolicy, TaskError, TaskSystem, TaskSystemConfig, ThreadHandle, ThreadId,
        ThreadSpec,
    };

    use super::*;

    fn commit_pi_wait<'lock>(
        system: &TaskSystem,
        lock: &'lock PiLockIdentity,
        waiter: ThreadId,
        owner: ThreadId,
    ) -> Result<PiWaitToken<'lock>, TaskError> {
        let registration = system.prepare_pi_wait_start(
            lock.lock_ref()?,
            waiter,
            PiWaitOwner::Owned(owner),
            waiter.as_u64(),
        )?;
        // SAFETY: scheduler-only tests model the local pinned waiter publication.
        Ok(unsafe { registration.commit_after_local_registration() })
    }

    #[test]
    fn local_waiter_ownership_list_does_not_duplicate_scheduler_order() {
        let mut queue = WaiterQueue::new();
        let fair = WaiterNode::new_for_test(thread(1));
        let rt = WaiterNode::new_for_test(thread(2));
        let deadline = WaiterNode::new_for_test(thread(3));

        unsafe {
            queue.insert(fair.as_ref());
            queue.insert(rt.as_ref());
            queue.insert(deadline.as_ref());
        }

        unsafe {
            assert_eq!(queue.pop_front().unwrap().thread_id(), thread(3));
            assert_eq!(queue.pop_front().unwrap().thread_id(), thread(2));
            assert_eq!(queue.pop_front().unwrap().thread_id(), thread(1));
        }
    }

    #[test]
    fn removes_a_known_local_waiter_without_scanning_for_scheduler_order() {
        let mut queue = WaiterQueue::new();
        let first = WaiterNode::new_for_test(thread(1));
        let middle = WaiterNode::new_for_test(thread(2));
        let last = WaiterNode::new_for_test(thread(3));

        unsafe {
            queue.insert(first.as_ref());
            queue.insert(middle.as_ref());
            queue.insert(last.as_ref());
        }
        let removed = queue
            .remove(&WaiterPointer::from_pin(middle.as_ref()))
            .unwrap();

        unsafe {
            assert_eq!(removed.thread_id(), thread(2));
            assert_eq!(queue.pop_front().unwrap().thread_id(), thread(3));
            assert_eq!(queue.pop_front().unwrap().thread_id(), thread(1));
        }
    }

    #[test]
    fn grant_is_visible_before_targeted_wake() {
        let waiter = WaiterNode::new_for_test(thread(1));

        assert!(!waiter.is_granted());
        waiter.grant();
        assert!(waiter.is_granted());
    }

    #[test]
    fn releasing_one_owned_lock_preserves_other_lock_donation() {
        let system = task_system(1);
        let owner = create_thread(&system, fair_policy());
        let low_donor = create_thread(&system, fifo_policy(20));
        let high_donor = create_thread(&system, fifo_policy(80));
        let low_lock = PiLockIdentity::new();
        let high_lock = PiLockIdentity::new();

        let low_wait = commit_pi_wait(&system, &low_lock, low_donor.id(), owner.id()).unwrap();
        assert_effective(&owner, fifo_policy(20));
        let high_wait = commit_pi_wait(&system, &high_lock, high_donor.id(), owner.id()).unwrap();
        assert_effective(&owner, fifo_policy(80));

        commit_test_transfer(&system, &high_lock, owner.id(), &high_wait);
        assert!(high_wait.is_granted());
        assert_effective(&owner, fifo_policy(20));

        commit_test_transfer(&system, &low_lock, owner.id(), &low_wait);
        assert!(low_wait.is_granted());
        assert_effective(&owner, fair_policy());
    }

    #[test]
    fn transitive_donation_propagates_and_withdraws_along_wait_chain() {
        let system = task_system(1);
        let first_owner = create_thread(&system, fair_policy());
        let second_owner = create_thread(&system, fifo_policy(30));
        let final_donor = create_thread(&system, fifo_policy(90));
        let first_lock = PiLockIdentity::new();
        let second_lock = PiLockIdentity::new();

        let middle_wait =
            commit_pi_wait(&system, &first_lock, second_owner.id(), first_owner.id()).unwrap();
        assert_effective(&first_owner, fifo_policy(30));
        let final_wait =
            commit_pi_wait(&system, &second_lock, final_donor.id(), second_owner.id()).unwrap();
        assert_effective(&second_owner, fifo_policy(90));
        assert_effective(&first_owner, fifo_policy(90));

        system.pi_wait_cancel(final_wait).unwrap();
        assert_effective(&second_owner, fifo_policy(30));
        assert_effective(&first_owner, fifo_policy(30));
        system.pi_wait_cancel(middle_wait).unwrap();
        assert_effective(&first_owner, fair_policy());
    }

    #[test]
    fn queued_remote_owner_boost_requests_its_cpu_reschedule() {
        let system = task_system(2);
        let mut remote_cpu = system.create_cpu_local(CpuId::new(1)).unwrap();
        system.bring_cpu_online(remote_cpu.as_mut()).unwrap();
        let owner = create_thread(&system, fair_policy());
        system.make_ready(owner.id()).unwrap();
        system.enqueue(remote_cpu.as_mut(), owner.id(), 0).unwrap();
        let donor = create_thread(&system, fifo_policy(70));
        let lock = PiLockIdentity::new();
        crate::test_runtime::reset_scheduler_ipis();

        let wait = commit_pi_wait(&system, &lock, donor.id(), owner.id()).unwrap();

        assert_effective(&owner, fifo_policy(70));
        assert_eq!(crate::test_runtime::scheduler_ipi_count(), 1);
        assert_eq!(crate::test_runtime::last_scheduler_ipi_cpu(), Some(1));
        let drained = system.drain_policy_updates(remote_cpu.as_mut(), 0).unwrap();
        assert_eq!(drained.drained(), 1);
        assert!(!drained.pending());
        system.pi_wait_cancel(wait).unwrap();
    }

    #[test]
    fn donation_cycle_is_rejected_before_the_scheduler_graph_changes() {
        let system = task_system(1);
        let first = create_thread(&system, fair_policy());
        let second = create_thread(&system, fair_policy());
        let first_lock = PiLockIdentity::new();
        let second_lock = PiLockIdentity::new();
        let first_wait = commit_pi_wait(&system, &first_lock, second.id(), first.id()).unwrap();

        assert!(matches!(
            commit_pi_wait(&system, &second_lock, first.id(), second.id()),
            Err(TaskError::PiCycle)
        ));
        system.pi_wait_cancel(first_wait).unwrap();
    }

    fn thread(slot: u32) -> ThreadId {
        ThreadId::from_parts(slot, 1)
    }

    fn task_system(cpu_count: usize) -> TaskSystem {
        TaskSystem::new(TaskSystemConfig::new(cpu_count)).unwrap()
    }

    fn commit_test_transfer(
        system: &TaskSystem,
        lock: &PiLockIdentity,
        old_owner: ThreadId,
        wait: &PiWaitToken<'_>,
    ) {
        let release = system
            .prepare_pi_mutex_release(lock.lock_ref().unwrap(), old_owner)
            .unwrap();
        // SAFETY: these scheduler-only tests model ownerless publication.
        drop(unsafe { release.commit_after_local_release() });
        let claim = system.prepare_pi_mutex_claim(wait).unwrap();
        // SAFETY: these scheduler-only tests model claimant publication.
        unsafe { claim.commit_after_local_claim() };
    }

    fn create_thread(system: &TaskSystem, policy: SchedulePolicy) -> ThreadHandle {
        system.create_thread(ThreadSpec::new(policy)).unwrap()
    }

    fn fair_policy() -> SchedulePolicy {
        SchedulePolicy::fair(Nice::ZERO, FairMode::Normal)
    }

    fn fifo_policy(priority: u8) -> SchedulePolicy {
        SchedulePolicy::fifo(RtPriority::new(priority).unwrap())
    }

    fn assert_effective(thread: &ThreadHandle, policy: SchedulePolicy) {
        assert_eq!(
            thread.effective_scheduling_key(),
            policy.scheduling_key(thread.id().as_u64())
        );
    }
}
