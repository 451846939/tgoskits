//! Priority-inheritance sleeping mutex.

use core::{
    pin::pin,
    sync::atomic::{AtomicU64, Ordering},
};

use ax_kernel_guard::NoPreempt as PreemptGuard;
use ax_kspin::SpinNoPreempt;
use ax_task::{
    PiLockIdentity, PiLockRef, PiWaitOwner, PiWaitToken, TaskError, ThreadHandle, ThreadId,
    current_needs_reschedule_pinned, current_thread_handle, current_thread_id_pinned,
    pi_block_current, pi_wake, prepare_pi_mutex_claim, prepare_pi_mutex_release,
    prepare_pi_wait_start, validate_blocking_context,
};

use crate::pi::{WaiterNode, WaiterPointer, WaiterQueue};

/// A non-recursive, urgency-ordered PI mutex implementing `lock_api::RawMutex`.
///
/// The uncontended path uses a Linux rtmutex-style atomic owner word. Its high
/// bit forces contenders through the metadata lock while waiter publication,
/// donation registration, and handoff are in progress. Blocking and targeted
/// wake happen after that metadata guard has been released.
pub struct RawMutex {
    owner: AtomicU64,
    metadata: SpinNoPreempt<MutexMetadata>,
    identity: PiLockIdentity,
    next_waiter_sequence: AtomicU64,
    #[cfg(test)]
    metadata_lock_acquisitions: AtomicU64,
    #[cfg(feature = "lockdep")]
    pub(crate) lockdep: crate::lockdep::LockdepMap,
}

#[derive(Debug)]
struct MutexMetadata {
    waiters: WaiterQueue,
}

#[cfg(test)]
std::thread_local! {
    static OWNERLESS_SELECTION_PROBE: core::cell::Cell<*const ()> =
        const { core::cell::Cell::new(core::ptr::null()) };
}

#[cfg(test)]
fn assert_ownerless_selection_is_published() {
    OWNERLESS_SELECTION_PROBE.with(|probe| {
        let token = probe.get();
        if token.is_null() {
            return;
        }
        let token = unsafe {
            // SAFETY: the regression test installs a pointer to its live token
            // only around the synchronous `unlock_pi` transition on this thread.
            &*token.cast::<PiWaitToken<'static>>()
        };
        assert!(
            token.is_selected(),
            "an ownerless PI owner word must not be observable before scheduler selection"
        );
    });
}

enum LockAttempt {
    Acquired,
    Contended,
}

#[derive(Clone, Copy)]
enum ContendedOwner {
    Unlocked,
    Owned(ThreadId),
    Ownerless,
}

const OWNER_HAS_WAITERS: u64 = 1 << 63;
const OWNER_ID_MASK: u64 = !OWNER_HAS_WAITERS;
const OWNER_SPIN_BATCH: usize = 64;

#[cfg(not(feature = "lockdep"))]
pub type LockSubclass = u32;
#[cfg(feature = "lockdep")]
pub type LockSubclass = crate::lockdep::LockSubclass;

/// Adds lockdep subclass acquisition to a sleeping [`Mutex`].
pub trait LockdepMutexExt<T: ?Sized> {
    /// Acquires the mutex using `subclass` for lock-order validation.
    fn lock_nested(&self, subclass: LockSubclass) -> MutexGuard<'_, T>;
}

impl<T: ?Sized> LockdepMutexExt<T> for Mutex<T> {
    #[inline(always)]
    #[track_caller]
    fn lock_nested(&self, subclass: LockSubclass) -> MutexGuard<'_, T> {
        #[cfg(not(feature = "lockdep"))]
        {
            let _ = subclass;
            self.lock()
        }

        #[cfg(feature = "lockdep")]
        {
            // SAFETY: the raw reference is used for the matching acquisition.
            let raw = unsafe { self.raw() };
            raw.lock_nested(subclass);
            // SAFETY: `lock_nested` acquired this mutex.
            unsafe { self.make_guard_unchecked() }
        }
    }
}

impl RawMutex {
    /// Creates an unlocked PI mutex.
    pub const fn new() -> Self {
        Self {
            owner: AtomicU64::new(0),
            metadata: SpinNoPreempt::new(MutexMetadata::new()),
            identity: PiLockIdentity::new(),
            next_waiter_sequence: AtomicU64::new(0),
            #[cfg(test)]
            metadata_lock_acquisitions: AtomicU64::new(0),
            #[cfg(feature = "lockdep")]
            lockdep: crate::lockdep::LockdepMap::new(),
        }
    }

    /// Returns whether the current thread owns this mutex.
    pub fn is_owned_by_current(&self) -> bool {
        let _preempt_guard = PreemptGuard::new();
        let current = pinned_current_thread_identity("query PI mutex ownership");
        owner_from_state(self.owner.load(Ordering::Acquire)) == Some(current)
    }

    #[inline(always)]
    fn lock_ref(&self) -> PiLockRef<'_> {
        task_result(self.identity.lock_ref(), "borrow PI mutex identity")
    }

    fn lock_metadata(&self) -> ax_kspin::SpinNoPreemptGuard<'_, MutexMetadata> {
        let metadata = self.metadata.lock();
        #[cfg(test)]
        self.metadata_lock_acquisitions
            .fetch_add(1, Ordering::Relaxed);
        metadata
    }

    fn lock_pi(&self) {
        let mut blocking_context_validated = false;

        loop {
            let (current, attempt) = self.try_or_observe_current();
            match attempt {
                LockAttempt::Acquired => return,
                LockAttempt::Contended => {
                    if !blocking_context_validated {
                        // The uncontended path neither publishes a waiter nor
                        // schedules and must remain usable during
                        // single-threaded boot.
                        task_result(
                            validate_blocking_context(),
                            "validate PI mutex sleep context",
                        );
                        blocking_context_validated = true;
                        continue;
                    }
                    self.lock_contended(current);
                    return;
                }
            }
        }
    }

    fn lock_contended(&self, current: ThreadId) {
        let current_handle = current_thread("register PI mutex waiter");
        assert_eq!(
            current_handle.id(),
            current,
            "PI mutex contender identity changed while preemption was disabled"
        );
        let sequence = self.next_waiter_sequence.fetch_add(1, Ordering::Relaxed);
        let waiter = pin!(WaiterNode::new(current));
        let token = {
            let mut metadata = self.lock_metadata();
            let owner = self.mark_waiters_and_owner();
            let owner = match owner {
                ContendedOwner::Unlocked => {
                    self.publish_owner(current, false);
                    return;
                }
                ContendedOwner::Owned(owner) if owner == current => {
                    panic!("thread attempted recursive PI mutex acquisition")
                }
                ContendedOwner::Owned(owner) => PiWaitOwner::Owned(owner),
                ContendedOwner::Ownerless => PiWaitOwner::Ownerless,
            };
            let registration = task_result(
                prepare_pi_wait_start(self.lock_ref(), owner, sequence),
                "prepare PI mutex wait",
            );
            // SAFETY: this lock call keeps the stack node pinned until handoff
            // removes it, and the slow-path gate owns the intrusive link.
            unsafe { metadata.waiters.insert(waiter.as_ref()) };
            // SAFETY: the matching pinned waiter is now owned by local mutex
            // metadata. The same gate keeps the physical owner word and PI
            // graph transaction indivisible to every other mutex slow path.
            unsafe { registration.commit_after_local_registration() }
        };
        if self.try_claim_waiter(&waiter, &token) {
            return;
        }
        self.wait_for_handoff(&waiter, token);
    }

    fn wait_for_handoff(&self, waiter: &core::pin::Pin<&mut WaiterNode>, token: PiWaitToken<'_>) {
        loop {
            if token.is_granted() {
                break;
            }
            if self.try_claim_waiter(waiter, &token) {
                break;
            }
            if !token.is_selected() && !self.spin_on_owner(&token) {
                task_result(pi_block_current(&token), "block on PI mutex");
            }
        }
        assert!(
            waiter.is_granted(),
            "scheduler PI grant became visible before the local claim"
        );
        assert_eq!(
            owner_from_state(self.owner.load(Ordering::Acquire)),
            Some(waiter.thread_id()),
            "local PI owner word must name the granted waiter"
        );
    }

    /// Spins only while the registered waiter can make progress under the same
    /// gates as Linux `rtmutex_spin_on_owner`: the observed owner is unchanged
    /// and executing, this waiter remains most urgent, and the current CPU has
    /// no pending reschedule request.
    fn spin_on_owner(&self, token: &PiWaitToken<'_>) -> bool {
        let _preempt_guard = PreemptGuard::new();
        let Some(owner) = token.initial_owner() else {
            return token.is_selected() || token.is_granted();
        };

        loop {
            if token.is_selected() || token.is_granted() {
                return true;
            }

            let state = self.owner.load(Ordering::Acquire);
            if owner_from_state(state) != Some(owner) {
                return token.is_selected() || token.is_granted();
            }
            let owner_on_cpu = token.initial_owner_is_on_cpu();
            // SAFETY: `_preempt_guard` pins this caller to one CPU throughout
            // the observation and the following spin batch.
            let need_resched = task_result(
                unsafe { current_needs_reschedule_pinned() },
                "read PI mutex reschedule state",
            );
            let waiter_is_top = token.is_top_waiter();

            if !owner_spin_eligible(state, owner, owner_on_cpu, waiter_is_top, need_resched) {
                return token.is_selected() || token.is_granted();
            }

            for _ in 0..OWNER_SPIN_BATCH {
                if token.is_selected() || token.is_granted() {
                    return true;
                }
                if owner_from_state(self.owner.load(Ordering::Acquire)) != Some(owner) {
                    return token.is_selected() || token.is_granted();
                }
                core::hint::spin_loop();
            }
        }
    }

    #[cfg(test)]
    fn try_or_observe_owner(&self, current: ThreadId) -> LockAttempt {
        self.try_or_observe_owner_word(current)
    }

    fn try_or_observe_current(&self) -> (ThreadId, LockAttempt) {
        let _preempt_guard = PreemptGuard::new();
        let current = pinned_current_thread_identity("lock PI mutex");
        let attempt = self.try_or_observe_owner_word(current);
        (current, attempt)
    }

    fn try_or_observe_owner_word(&self, current: ThreadId) -> LockAttempt {
        let current = owner_word(current);
        match self
            .owner
            .compare_exchange(0, current, Ordering::Acquire, Ordering::Relaxed)
        {
            Ok(_) => LockAttempt::Acquired,
            Err(owner) if owner & OWNER_ID_MASK == current => {
                panic!("thread attempted recursive PI mutex acquisition")
            }
            Err(_) => LockAttempt::Contended,
        }
    }

    fn try_lock_pi(&self) -> bool {
        let _preempt_guard = PreemptGuard::new();
        let current = pinned_current_thread_identity("try PI mutex");
        self.owner
            .compare_exchange(0, owner_word(current), Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
    }

    fn try_claim_waiter(
        &self,
        waiter: &core::pin::Pin<&mut WaiterNode>,
        token: &PiWaitToken<'_>,
    ) -> bool {
        if token.is_granted() {
            return true;
        }
        if waiter.thread_id() != token.thread_id() {
            return false;
        }
        let claimant = token.thread_id();
        {
            let mut metadata = self.lock_metadata();
            if self.owner.load(Ordering::Acquire) != OWNER_HAS_WAITERS || !token.is_selected() {
                return token.is_granted();
            }
            let claim = task_result(
                prepare_pi_mutex_claim(token),
                "prepare ownerless PI mutex claim",
            );
            let selected = metadata
                .waiters
                .remove(&WaiterPointer::from_pin(waiter.as_ref()))
                .expect("eligible PI claimant must remain locally queued");
            self.publish_owner(claimant, !metadata.waiters.is_empty());
            // SAFETY: the local waiter remains pinned in this lock call until
            // it observes both the local and scheduler publications.
            unsafe { selected.grant() };
            // SAFETY: local ownership and grant are visible while the one
            // slow-path gate still excludes registration and release.
            unsafe { claim.commit_after_local_claim() };
        }
        true
    }

    #[cfg(test)]
    fn try_claim_waiter_in_model(
        &self,
        system: &ax_task::TaskSystem,
        waiter: &core::pin::Pin<&mut WaiterNode>,
        token: &PiWaitToken<'_>,
    ) -> bool {
        if token.is_granted() {
            return true;
        }
        if waiter.thread_id() != token.thread_id() {
            return false;
        }
        let claimant = token.thread_id();
        let mut metadata = self.lock_metadata();
        if self.owner.load(Ordering::Acquire) != OWNER_HAS_WAITERS || !token.is_selected() {
            return token.is_granted();
        }
        let claim = task_result(
            system.prepare_pi_mutex_claim(token),
            "prepare modeled ownerless PI mutex claim",
        );
        let selected = metadata
            .waiters
            .remove(&WaiterPointer::from_pin(waiter.as_ref()))
            .expect("eligible modeled PI claimant must remain locally queued");
        self.publish_owner(claimant, !metadata.waiters.is_empty());
        // SAFETY: the pinned waiter remains live through this modeled claim.
        unsafe { selected.grant() };
        // SAFETY: the model holds the same slow-path gate as production.
        unsafe { claim.commit_after_local_claim() };
        true
    }

    fn unlock_pi(&self) {
        let current = {
            let _preempt_guard = PreemptGuard::new();
            pinned_current_thread_identity("unlock PI mutex")
        };
        if self
            .owner
            .compare_exchange(owner_word(current), 0, Ordering::Release, Ordering::Relaxed)
            .is_ok()
        {
            return;
        }

        self.unlock_contended(current);
    }

    fn unlock_contended(&self, current: ThreadId) {
        // Keep this CPU pinned from owner deboost through the deferred targeted
        // wake, matching Linux's rtmutex wake_q handoff.
        let _preempt_guard = PreemptGuard::new();
        let wake = {
            let metadata = self.lock_metadata();
            assert_eq!(
                owner_from_state(self.owner.load(Ordering::Acquire)),
                Some(current),
                "thread attempted to unlock a PI mutex it does not own"
            );
            if metadata.waiters.is_empty() {
                self.owner.store(0, Ordering::Release);
                return;
            }
            let scheduler_release = task_result(
                prepare_pi_mutex_release(self.lock_ref()),
                "prepare PI mutex release",
            );
            self.owner.store(OWNER_HAS_WAITERS, Ordering::Release);
            // SAFETY: the slow-path gate still excludes registration and claim
            // while the scheduler publishes the ownerless selection.
            let wake = unsafe { scheduler_release.commit_after_local_release() };
            #[cfg(test)]
            assert_ownerless_selection_is_published();
            wake
        };
        task_result(pi_wake(&wake), "wake selected PI mutex waiter");
    }

    /// Marks the owner word as contended while holding the waiter metadata
    /// lock and returns the owner that must receive the donation.
    ///
    /// A concurrent fast unlock can win immediately before the `fetch_or`.
    /// In that case the transitional waiters-only state excludes new fast
    /// lockers until this caller publishes itself as the owner.
    fn mark_waiters_and_owner(&self) -> ContendedOwner {
        let previous = self.owner.fetch_or(OWNER_HAS_WAITERS, Ordering::AcqRel);
        if let Some(owner) = owner_from_state(previous) {
            ContendedOwner::Owned(owner)
        } else if previous & OWNER_HAS_WAITERS == 0 {
            ContendedOwner::Unlocked
        } else {
            ContendedOwner::Ownerless
        }
    }

    fn publish_owner(&self, owner: ThreadId, has_waiters: bool) {
        let state = owner_word(owner) | if has_waiters { OWNER_HAS_WAITERS } else { 0 };
        self.owner.store(state, Ordering::Release);
    }

    #[cfg(feature = "lockdep")]
    #[track_caller]
    fn lock_nested(&self, subclass: LockSubclass) {
        let lockdep = crate::lockdep::LockdepAcquire::prepare_nested(self, false, subclass);
        self.lock_pi();
        lockdep.finish(true);
    }

    #[cfg(feature = "lockdep")]
    #[track_caller]
    fn try_lock_nested(&self, subclass: LockSubclass) -> bool {
        let lockdep = crate::lockdep::LockdepAcquire::prepare_nested(self, true, subclass);
        let acquired = self.try_lock_pi();
        lockdep.finish(acquired);
        acquired
    }
}

impl MutexMetadata {
    const fn new() -> Self {
        Self {
            waiters: WaiterQueue::new(),
        }
    }
}

impl Default for RawMutex {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: task-context metadata transitions are serialized by a preemption-safe
// gate. Hard IRQ paths never access this state. A lock_api guard is created only
// after scheduler ownership registration or an explicit PI handoff grants the
// calling thread.
unsafe impl lock_api::RawMutex for RawMutex {
    type GuardMarker = lock_api::GuardNoSend;

    const INIT: Self = Self::new();

    #[inline(always)]
    #[track_caller]
    fn lock(&self) {
        #[cfg(feature = "lockdep")]
        self.lock_nested(ax_lockdep::DEFAULT_LOCK_SUBCLASS);

        #[cfg(not(feature = "lockdep"))]
        self.lock_pi();
    }

    #[inline(always)]
    #[track_caller]
    fn try_lock(&self) -> bool {
        #[cfg(feature = "lockdep")]
        {
            self.try_lock_nested(ax_lockdep::DEFAULT_LOCK_SUBCLASS)
        }

        #[cfg(not(feature = "lockdep"))]
        {
            self.try_lock_pi()
        }
    }

    #[inline(always)]
    unsafe fn unlock(&self) {
        #[cfg(feature = "lockdep")]
        crate::lockdep::release(self);
        self.unlock_pi();
    }

    #[inline(always)]
    fn is_locked(&self) -> bool {
        self.owner.load(Ordering::Relaxed) != 0
    }
}

fn current_thread(operation: &'static str) -> ThreadHandle {
    task_result(current_thread_handle(), operation)
}

fn pinned_current_thread_identity(operation: &'static str) -> ThreadId {
    // SAFETY: every caller retains either a NoPreempt guard or the PI metadata
    // SpinNoPreempt guard through the matching owner-word transition.
    task_result(unsafe { current_thread_id_pinned() }, operation)
}

fn owner_word(thread: ThreadId) -> u64 {
    let raw = thread.as_u64();
    assert_eq!(
        raw & OWNER_HAS_WAITERS,
        0,
        "PI mutex owner identity exceeds the owner-word generation range"
    );
    raw
}

fn owner_from_state(state: u64) -> Option<ThreadId> {
    let raw = state & OWNER_ID_MASK;
    (raw != 0).then(|| ThreadId::from_parts(raw as u32, (raw >> 32) as u32))
}

fn owner_spin_eligible(
    state: u64,
    owner: ThreadId,
    owner_on_cpu: bool,
    waiter_is_top: bool,
    need_resched: bool,
) -> bool {
    owner_from_state(state) == Some(owner) && owner_on_cpu && waiter_is_top && !need_resched
}

#[track_caller]
fn task_result<T>(result: Result<T, TaskError>, operation: &'static str) -> T {
    result.unwrap_or_else(|error| panic!("{operation} failed: {error}"))
}

/// A safe PI mutex using [`RawMutex`].
pub type Mutex<T> = lock_api::Mutex<RawMutex, T>;
/// A non-send guard returned by [`Mutex`].
pub type MutexGuard<'a, T> = lock_api::MutexGuard<'a, RawMutex, T>;

#[cfg(test)]
mod tests {
    use alloc::boxed::Box;
    use core::{mem::MaybeUninit, pin::Pin};

    use ax_task::{
        CpuId, PiLockRef, PiWaitOwner, PiWaitToken, SchedulePolicy, TaskSystem, TaskSystemConfig,
        ThreadSpec,
    };

    use super::*;

    fn commit_pi_wait<'lock>(
        system: &TaskSystem,
        lock: PiLockRef<'lock>,
        waiter: ThreadId,
        owner: ThreadId,
    ) -> Result<PiWaitToken<'lock>, TaskError> {
        let registration = system.prepare_pi_wait_start(
            lock,
            waiter,
            PiWaitOwner::Owned(owner),
            waiter.as_u64(),
        )?;
        // SAFETY: scheduler-only tests model the local pinned waiter publication.
        Ok(unsafe { registration.commit_after_local_registration() })
    }

    #[test]
    fn raw_mutex_guard_is_not_send() {
        fn assert_marker<R: lock_api::RawMutex<GuardMarker = lock_api::GuardNoSend>>() {}
        assert_marker::<RawMutex>();
    }

    #[test]
    fn metadata_starts_unowned_and_ready() {
        let raw = RawMutex::new();
        assert_eq!(owner_from_state(raw.owner.load(Ordering::Relaxed)), None);
        assert!(raw.metadata.lock().waiters.is_empty());
    }

    #[test]
    fn owner_spin_requires_linux_rtmutex_progress_gates() {
        let owner = ThreadId::from_parts(3, 7);
        let owned = owner_word(owner) | OWNER_HAS_WAITERS;

        assert!(owner_spin_eligible(owned, owner, true, true, false));
        assert!(!owner_spin_eligible(
            owner_word(ThreadId::from_parts(4, 7)),
            owner,
            true,
            true,
            false,
        ));
        assert!(!owner_spin_eligible(owned, owner, false, true, false));
        assert!(!owner_spin_eligible(owned, owner, true, false, false));
        assert!(!owner_spin_eligible(owned, owner, true, true, true));
    }

    #[test]
    fn reconstructed_mutex_does_not_reuse_the_previous_pi_identity() {
        let mut storage = MaybeUninit::<RawMutex>::uninit();
        let pointer = storage.as_mut_ptr();

        let (first, second) = unsafe {
            // SAFETY: each write begins a fresh RawMutex lifetime in the same
            // storage, and each initialized value is dropped exactly once.
            pointer.write(RawMutex::new());
            let first = (&*pointer).lock_ref().id();
            pointer.drop_in_place();
            pointer.write(RawMutex::new());
            let second = (&*pointer).lock_ref().id();
            pointer.drop_in_place();
            (first, second)
        };

        assert_ne!(
            first, second,
            "a stale scheduler edge must not alias a reconstructed mutex"
        );
    }

    #[test]
    fn pi_identity_stays_stable_for_one_mutex_lifetime() {
        let raw = RawMutex::new();

        assert_eq!(raw.lock_ref().id(), raw.lock_ref().id());
    }

    #[test]
    fn uncontended_owner_acquisition_stays_local() {
        let (system, cpu) = install_current_thread();
        let _runtime = crate::test_runtime::install(
            (&*system as *const TaskSystem).expose_provenance(),
            (cpu.as_ref().get_ref() as *const ax_task::CpuLocal).expose_provenance(),
        );
        let raw = RawMutex::new();
        let current = current_thread("test new PI owner registration");

        let attempt = raw.try_or_observe_owner(current.id());

        assert!(matches!(attempt, LockAttempt::Acquired));
        assert_eq!(crate::test_runtime::preempt_depth(), 0);
        assert_eq!(
            owner_from_state(raw.owner.load(Ordering::Acquire)),
            Some(current.id())
        );
        crate::test_runtime::clear();
    }

    #[test]
    fn scheduler_grant_is_not_visible_before_local_handoff_publication() {
        let (system, mut cpu) = install_current_thread();
        let _runtime = crate::test_runtime::install(
            (&*system as *const TaskSystem).expose_provenance(),
            (cpu.as_ref().get_ref() as *const ax_task::CpuLocal).expose_provenance(),
        );
        let raw = RawMutex::new();
        let owner = current_thread("test PI handoff owner");
        let waiter_thread = system
            .create_thread(ThreadSpec::new(SchedulePolicy::default()))
            .unwrap();
        system.make_ready(waiter_thread.id()).unwrap();
        system.enqueue(cpu.as_mut(), waiter_thread.id(), 0).unwrap();
        let token =
            commit_pi_wait(&system, raw.lock_ref(), waiter_thread.id(), owner.id()).unwrap();
        let waiter = pin!(WaiterNode::new(waiter_thread.id()));
        {
            let mut metadata = raw.metadata.lock();
            raw.publish_owner(owner.id(), true);
            // SAFETY: this pinned waiter remains live until the test removes it
            // through the handoff below.
            unsafe { metadata.waiters.insert(waiter.as_ref()) };
        }

        {
            let _metadata = raw.metadata.lock();
            let release = system
                .prepare_pi_mutex_release(raw.lock_ref(), owner.id())
                .unwrap();
            assert_eq!(release.selected(), waiter_thread.id());
            assert!(!token.is_selected());
            raw.owner.store(OWNER_HAS_WAITERS, Ordering::Release);
            // SAFETY: the ownerless local state and scheduler selection are
            // committed while the slow-path gate remains held.
            drop(unsafe { release.commit_after_local_release() });
            assert!(token.is_selected());
        }

        {
            let mut metadata = raw.metadata.lock();
            let claim = system.prepare_pi_mutex_claim(&token).unwrap();
            let selected = metadata
                .waiters
                .remove(&WaiterPointer::from_pin(waiter.as_ref()))
                .unwrap();
            raw.publish_owner(waiter_thread.id(), false);
            // SAFETY: this pinned test waiter remains live through the complete
            // scheduler transaction.
            unsafe { selected.grant() };
            assert!(waiter.is_granted());
            assert!(!token.is_granted());
            // SAFETY: local owner and waiter grant were published under the
            // same gate before the scheduler claim commit.
            unsafe { claim.commit_after_local_claim() };
            assert!(token.is_granted());
        }

        assert!(token.is_granted());
        assert!(waiter.is_granted());
        assert_eq!(
            owner_from_state(raw.owner.load(Ordering::Acquire)),
            Some(waiter_thread.id())
        );
        assert_eq!(crate::test_runtime::preempt_depth(), 0);
        crate::test_runtime::clear();
    }

    #[test]
    fn contended_unlock_wakes_an_ownerless_claimant() {
        let (system, mut cpu) = install_current_thread();
        let _runtime = crate::test_runtime::install(
            (&*system as *const TaskSystem).expose_provenance(),
            (cpu.as_ref().get_ref() as *const ax_task::CpuLocal).expose_provenance(),
        );
        let raw = RawMutex::new();
        let owner = current_thread("test PI release owner");
        let waiter_thread = system
            .create_thread(ThreadSpec::new(SchedulePolicy::default()))
            .unwrap();
        system.make_ready(waiter_thread.id()).unwrap();
        system.enqueue(cpu.as_mut(), waiter_thread.id(), 0).unwrap();
        let token =
            commit_pi_wait(&system, raw.lock_ref(), waiter_thread.id(), owner.id()).unwrap();
        let waiter = pin!(WaiterNode::new(waiter_thread.id()));
        {
            let mut metadata = raw.metadata.lock();
            raw.publish_owner(owner.id(), true);
            // SAFETY: the pinned waiter stays alive until the assertions below
            // have observed the complete release transition.
            unsafe { metadata.waiters.insert(waiter.as_ref()) };
        }

        raw.unlock_pi();

        let state = raw.owner.load(Ordering::Acquire);
        assert_eq!(
            owner_from_state(state),
            None,
            "Linux rtmutex release must not name a sleeping waiter as owner"
        );
        assert_ne!(
            state & OWNER_HAS_WAITERS,
            0,
            "the ownerless state must keep newcomers on the serialized claim path"
        );
        assert!(
            !token.is_granted(),
            "wake selection is not ownership until the waiter claims locally"
        );
        assert!(
            !waiter.is_granted(),
            "the local waiter must claim after wake instead of receiving ownership on unlock"
        );
        assert!(raw.try_claim_waiter_in_model(&system, &waiter, &token));
        crate::test_runtime::clear();
    }

    #[test]
    fn ownerless_publication_includes_scheduler_selection() {
        let (system, mut cpu) = install_current_thread();
        let _runtime = crate::test_runtime::install(
            (&*system as *const TaskSystem).expose_provenance(),
            (cpu.as_ref().get_ref() as *const ax_task::CpuLocal).expose_provenance(),
        );
        let raw = RawMutex::new();
        let owner = current_thread("test atomic PI handoff owner");
        let waiter_thread = system
            .create_thread(ThreadSpec::new(SchedulePolicy::default()))
            .unwrap();
        system.make_ready(waiter_thread.id()).unwrap();
        system.enqueue(cpu.as_mut(), waiter_thread.id(), 0).unwrap();
        let token =
            commit_pi_wait(&system, raw.lock_ref(), waiter_thread.id(), owner.id()).unwrap();
        let waiter = pin!(WaiterNode::new(waiter_thread.id()));
        {
            let mut metadata = raw.metadata.lock();
            raw.publish_owner(owner.id(), true);
            // SAFETY: the waiter remains pinned until it claims the handoff.
            unsafe { metadata.waiters.insert(waiter.as_ref()) };
        }

        OWNERLESS_SELECTION_PROBE.with(|probe| {
            probe.set((&token as *const PiWaitToken<'_>).cast());
        });
        raw.unlock_pi();
        OWNERLESS_SELECTION_PROBE.with(|probe| probe.set(core::ptr::null()));

        assert!(token.is_selected());
        assert!(raw.try_claim_waiter_in_model(&system, &waiter, &token));
        crate::test_runtime::clear();
    }

    #[test]
    fn selected_waiter_claims_only_after_ownerless_release() {
        let (system, mut cpu) = install_current_thread();
        let _runtime = crate::test_runtime::install(
            (&*system as *const TaskSystem).expose_provenance(),
            (cpu.as_ref().get_ref() as *const ax_task::CpuLocal).expose_provenance(),
        );
        let raw = RawMutex::new();
        let owner = current_thread("test PI release owner");
        let waiter_thread = system
            .create_thread(ThreadSpec::new(SchedulePolicy::default()))
            .unwrap();
        system.make_ready(waiter_thread.id()).unwrap();
        system.enqueue(cpu.as_mut(), waiter_thread.id(), 0).unwrap();
        let token =
            commit_pi_wait(&system, raw.lock_ref(), waiter_thread.id(), owner.id()).unwrap();
        let waiter = pin!(WaiterNode::new(waiter_thread.id()));
        {
            let mut metadata = raw.metadata.lock();
            raw.publish_owner(owner.id(), true);
            // SAFETY: the waiter remains pinned through release and claim.
            unsafe { metadata.waiters.insert(waiter.as_ref()) };
        }

        raw.unlock_pi();
        assert!(token.is_selected());
        assert!(!token.is_granted());

        assert!(raw.try_claim_waiter_in_model(&system, &waiter, &token));

        assert!(token.is_granted());
        assert!(waiter.is_granted());
        assert_eq!(
            owner_from_state(raw.owner.load(Ordering::Acquire)),
            Some(waiter_thread.id())
        );
        assert!(raw.metadata.lock().waiters.is_empty());
        crate::test_runtime::clear();
    }

    #[test]
    fn equal_fair_waiter_cannot_steal_ownerless_pi_mutex() {
        let (system, mut cpu) = install_current_thread();
        let _runtime = crate::test_runtime::install(
            (&*system as *const TaskSystem).expose_provenance(),
            (cpu.as_ref().get_ref() as *const ax_task::CpuLocal).expose_provenance(),
        );
        let raw = RawMutex::new();
        let owner = current_thread("test PI lateral-steal owner");
        let first_thread = system
            .create_thread(ThreadSpec::new(SchedulePolicy::default()))
            .unwrap();
        let second_thread = system
            .create_thread(ThreadSpec::new(SchedulePolicy::default()))
            .unwrap();
        for thread in [&first_thread, &second_thread] {
            system.make_ready(thread.id()).unwrap();
            system.enqueue(cpu.as_mut(), thread.id(), 0).unwrap();
        }
        let first_token =
            commit_pi_wait(&system, raw.lock_ref(), first_thread.id(), owner.id()).unwrap();
        let second_token =
            commit_pi_wait(&system, raw.lock_ref(), second_thread.id(), owner.id()).unwrap();
        let first = pin!(WaiterNode::new(first_thread.id()));
        let second = pin!(WaiterNode::new(second_thread.id()));
        {
            let mut metadata = raw.metadata.lock();
            raw.publish_owner(owner.id(), true);
            // SAFETY: both waiters remain pinned through release and claim.
            unsafe {
                metadata.waiters.insert(first.as_ref());
                metadata.waiters.insert(second.as_ref());
            }
        }

        raw.unlock_pi();
        assert!(first_token.is_selected());
        assert!(!second_token.is_selected());

        assert!(!raw.try_claim_waiter_in_model(&system, &second, &second_token));
        assert!(raw.try_claim_waiter_in_model(&system, &first, &first_token));

        assert!(!second_token.is_granted());
        assert!(!first_token.is_selected());
        assert!(first_token.is_granted());
        assert_eq!(
            owner_from_state(raw.owner.load(Ordering::Acquire)),
            Some(first_thread.id())
        );
        system.pi_wait_cancel(second_token).unwrap();
        crate::test_runtime::clear();
    }

    #[test]
    fn uncontended_unlock_releases_without_scheduler_registration() {
        let (system, cpu) = install_current_thread();
        let _runtime = crate::test_runtime::install(
            (&*system as *const TaskSystem).expose_provenance(),
            (cpu.as_ref().get_ref() as *const ax_task::CpuLocal).expose_provenance(),
        );
        let raw = RawMutex::new();
        let owner = current_thread("test uncontended PI owner");
        let contender = system
            .create_thread(ThreadSpec::new(SchedulePolicy::default()))
            .unwrap();
        raw.publish_owner(owner.id(), false);

        raw.unlock_pi();
        assert!(matches!(
            raw.try_or_observe_owner(contender.id()),
            LockAttempt::Acquired
        ));
        assert_eq!(
            owner_from_state(raw.owner.load(Ordering::Acquire)),
            Some(contender.id())
        );
        crate::test_runtime::clear();
    }

    #[test]
    fn uncontended_lock_keeps_ownership_local() {
        let (system, cpu) = install_current_thread();
        let _runtime = crate::test_runtime::install(
            (&*system as *const TaskSystem).expose_provenance(),
            (cpu.as_ref().get_ref() as *const ax_task::CpuLocal).expose_provenance(),
        );
        let mutex = Mutex::new(7usize);

        let guard = mutex.lock();
        assert!(mutex.is_locked());
        assert!(
            // SAFETY: this test only inspects the raw mutex while retaining the
            // safe guard and performs no raw lock operation.
            unsafe { mutex.raw() }.is_owned_by_current()
        );
        assert_eq!(*guard, 7);
        drop(guard);

        assert!(!mutex.is_locked());
        crate::test_runtime::clear();
    }

    #[test]
    fn uncontended_lock_does_not_enter_scheduler_irq_facade() {
        const ITERATIONS: usize = 128;

        let (system, cpu) = install_current_thread();
        let _runtime = crate::test_runtime::install(
            (&*system as *const TaskSystem).expose_provenance(),
            (cpu.as_ref().get_ref() as *const ax_task::CpuLocal).expose_provenance(),
        );
        let mutex = Mutex::new(0usize);

        for _ in 0..ITERATIONS {
            let mut guard = mutex.lock();
            *guard += 1;
        }

        assert_eq!(
            crate::test_runtime::irq_guard_entries(),
            0,
            "Linux rtmutex-style uncontended lock/unlock must remain on the owner-word fast path \
             without entering the scheduler IRQ facade"
        );
        assert_eq!(*mutex.lock(), ITERATIONS);
        crate::test_runtime::clear();
    }

    #[test]
    fn uncontended_lock_does_not_touch_pi_waiter_metadata() {
        const ITERATIONS: usize = 128;

        let (system, cpu) = install_current_thread();
        let _runtime = crate::test_runtime::install(
            (&*system as *const TaskSystem).expose_provenance(),
            (cpu.as_ref().get_ref() as *const ax_task::CpuLocal).expose_provenance(),
        );
        let mutex = Mutex::new(0usize);
        // SAFETY: the raw mutex remains owned by `mutex` for the whole test.
        let raw = unsafe { mutex.raw() };
        raw.metadata_lock_acquisitions.store(0, Ordering::Relaxed);

        for _ in 0..ITERATIONS {
            let mut guard = mutex.lock();
            *guard += 1;
        }

        assert_eq!(
            raw.metadata_lock_acquisitions.load(Ordering::Relaxed),
            0,
            "Linux rtmutex-style uncontended lock/unlock must not touch waiter metadata"
        );
        crate::test_runtime::clear();
    }

    #[test]
    fn uncontended_lock_does_not_require_a_blocking_context() {
        let (system, cpu) = install_current_thread();
        let _runtime = crate::test_runtime::install(
            (&*system as *const TaskSystem).expose_provenance(),
            (cpu.as_ref().get_ref() as *const ax_task::CpuLocal).expose_provenance(),
        );
        crate::test_runtime::set_schedule_context_safe(false);
        let mutex = Mutex::new(7usize);

        let guard = mutex.lock();
        assert_eq!(*guard, 7);
        drop(guard);

        assert!(!mutex.is_locked());
        crate::test_runtime::clear();
    }

    #[test]
    fn contended_lock_rejects_an_unsafe_blocking_context_before_publication() {
        let (system, cpu) = install_current_thread();
        let _runtime = crate::test_runtime::install(
            (&*system as *const TaskSystem).expose_provenance(),
            (cpu.as_ref().get_ref() as *const ax_task::CpuLocal).expose_provenance(),
        );
        let raw = RawMutex::new();
        let owner = system
            .create_thread(ThreadSpec::new(SchedulePolicy::default()))
            .unwrap();
        raw.publish_owner(owner.id(), false);
        crate::test_runtime::set_schedule_context_safe(false);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            lock_api::RawMutex::lock(&raw);
        }));

        assert!(result.is_err());
        assert!(raw.metadata.lock().waiters.is_empty());
        assert_eq!(crate::test_runtime::preempt_depth(), 0);
        crate::test_runtime::clear();
    }

    #[test]
    fn try_lock_success_and_failure_restore_preemption() {
        let (system, cpu) = install_current_thread();
        let _runtime = crate::test_runtime::install(
            (&*system as *const TaskSystem).expose_provenance(),
            (cpu.as_ref().get_ref() as *const ax_task::CpuLocal).expose_provenance(),
        );
        let raw = RawMutex::new();

        assert!(lock_api::RawMutex::try_lock(&raw));
        assert_eq!(crate::test_runtime::preempt_depth(), 0);
        assert!(!lock_api::RawMutex::try_lock(&raw));
        assert_eq!(crate::test_runtime::preempt_depth(), 0);

        // SAFETY: the first successful try-lock acquisition remains owned by
        // this test thread.
        unsafe { lock_api::RawMutex::unlock(&raw) };
        assert_eq!(crate::test_runtime::preempt_depth(), 0);
        crate::test_runtime::clear();
    }

    fn install_current_thread() -> (Box<TaskSystem>, Pin<Box<ax_task::CpuLocal>>) {
        let system = Box::new(
            TaskSystem::new(TaskSystemConfig::new(1)).expect("test task system should initialize"),
        );
        let mut cpu = system
            .create_cpu_local(CpuId::new(0))
            .expect("test CPU should be valid");
        system
            .install_bootstrap_thread(cpu.as_mut(), ThreadSpec::new(SchedulePolicy::default()))
            .expect("bootstrap thread should install");
        system
            .bring_cpu_online(cpu.as_mut())
            .expect("test CPU should come online");
        (system, cpu)
    }
}
