//! Priority-inheritance mutex identities and wait handshake tokens.

use alloc::sync::Arc;
use core::{
    marker::PhantomData,
    ptr::NonNull,
    sync::atomic::{AtomicU64, Ordering},
};

use crate::{
    PiWaitStateError, PiWaitTree, TaskError, ThreadCore, ThreadId,
    lock::{RawTicketGuard, RawTicketLock},
};

static NEXT_PI_MUTEX_GENERATION: AtomicU64 = AtomicU64::new(1);
const OWNER_HAS_WAITERS: u64 = 1 << 63;
const OWNER_ID_MASK: u64 = !OWNER_HAS_WAITERS;

/// Stable identity of one kernel PI mutex.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PiMutexId(u64);

impl PiMutexId {
    /// Returns the globally unique generation allocated to this lock instance.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Lazily allocated identity owned by one physical PI lock instance.
///
/// Keeping the allocator state inside the lock makes reconstructing a lock at
/// the same address allocate a fresh generation. The generation is never
/// reused, but the physical lock owner must still quiesce all scheduler wait
/// registrations before destruction.
pub struct PiMutexCore {
    owner: AtomicU64,
    generation: AtomicU64,
    state: RawTicketLock<PiMutexWaiters>,
}

impl PiMutexCore {
    /// Creates an identity owner that has not yet entered the PI scheduler.
    pub const fn new() -> Self {
        Self {
            owner: AtomicU64::new(0),
            generation: AtomicU64::new(0),
            state: RawTicketLock::new(PiMutexWaiters::new()),
        }
    }

    /// Attempts the atomic uncontended acquisition path.
    pub fn try_acquire(&self, current: ThreadId) -> Result<PiMutexAcquire, TaskError> {
        let current_word = owner_word(current)?;
        match self
            .owner
            .compare_exchange(0, current_word, Ordering::Acquire, Ordering::Relaxed)
        {
            Ok(_) => Ok(PiMutexAcquire::Acquired),
            Err(owner) if owner & OWNER_ID_MASK == current_word => Err(
                TaskError::InvalidPiWaitState(PiWaitStateError::WaiterOwnsLock),
            ),
            Err(_) => Ok(PiMutexAcquire::Contended),
        }
    }

    /// Attempts the atomic uncontended release path.
    pub fn try_release(&self, current: ThreadId) -> Result<bool, TaskError> {
        let current_word = owner_word(current)?;
        match self
            .owner
            .compare_exchange(current_word, 0, Ordering::Release, Ordering::Relaxed)
        {
            Ok(_) => Ok(true),
            Err(owner) if owner_from_word(owner) == Some(current) => Ok(false),
            Err(_) => Err(TaskError::InvalidPiState),
        }
    }

    /// Returns whether `current` is the physical mutex owner.
    pub fn is_owned_by(&self, current: ThreadId) -> bool {
        owner_from_word(self.owner.load(Ordering::Acquire)) == Some(current)
    }

    /// Returns whether the mutex is owned or in an ownerless handoff window.
    pub fn is_locked(&self) -> bool {
        self.owner.load(Ordering::Relaxed) != 0
    }

    /// Borrows this physical lock's stable scheduler identity.
    ///
    /// The returned borrow keeps the embedded waiter tree at a stable address
    /// for every scheduler token derived from it. Moving or destroying the
    /// mutex while a wait token exists is therefore rejected by Rust's normal
    /// borrow rules rather than delegated to a raw-address convention.
    pub fn mutex_ref(&self) -> Result<PiMutexRef<'_>, TaskError> {
        let observed = self.generation.load(Ordering::Acquire);
        if observed != 0 {
            return Ok(PiMutexRef {
                core: self,
                id: PiMutexId(observed),
            });
        }

        let allocated = NEXT_PI_MUTEX_GENERATION
            .try_update(Ordering::AcqRel, Ordering::Acquire, |next| {
                next.checked_add(1)
            })
            .map(PiMutexId)
            .map_err(|_| TaskError::InvalidPiState)?;
        match self
            .generation
            .compare_exchange(0, allocated.0, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => Ok(PiMutexRef {
                core: self,
                id: allocated,
            }),
            Err(installed) if installed != 0 => Ok(PiMutexRef {
                core: self,
                id: PiMutexId(installed),
            }),
            Err(_) => Err(TaskError::InvalidPiState),
        }
    }

    pub(crate) fn owner_snapshot(&self) -> PiMutexOwnerSnapshot {
        let word = self.owner.load(Ordering::Acquire);
        PiMutexOwnerSnapshot {
            word,
            owner: owner_from_word(word),
        }
    }

    pub(crate) fn try_acquire_snapshot(
        &self,
        snapshot: PiMutexOwnerSnapshot,
        current: ThreadId,
    ) -> Result<bool, TaskError> {
        debug_assert_eq!(snapshot.word, 0);
        let current = owner_word(current)?;
        Ok(self
            .owner
            .compare_exchange(snapshot.word, current, Ordering::Acquire, Ordering::Relaxed)
            .is_ok())
    }

    pub(crate) fn try_mark_waiters(&self, snapshot: PiMutexOwnerSnapshot) -> bool {
        if snapshot.has_waiters() {
            return self.owner.load(Ordering::Acquire) == snapshot.word;
        }
        self.owner
            .compare_exchange(
                snapshot.word,
                snapshot.word | OWNER_HAS_WAITERS,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    pub(crate) fn publish_owner(&self, owner: ThreadId, has_waiters: bool) {
        let owner = owner_word(owner).expect("validated thread identity must fit PI owner word");
        self.owner.store(
            owner | if has_waiters { OWNER_HAS_WAITERS } else { 0 },
            Ordering::Release,
        );
    }

    pub(crate) fn publish_ownerless(&self) {
        self.owner.store(OWNER_HAS_WAITERS, Ordering::Release);
    }

    pub(crate) fn clear_waiters_bit(&self, owner: ThreadId) -> Result<(), TaskError> {
        let owner = owner_word(owner)?;
        self.owner.store(owner, Ordering::Release);
        Ok(())
    }
}

impl core::fmt::Debug for PiMutexCore {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("PiMutexCore")
            .field(
                "owner",
                &owner_from_word(self.owner.load(Ordering::Relaxed)),
            )
            .field("generation", &self.generation.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl Drop for PiMutexCore {
    fn drop(&mut self) {
        let state = self.state.lock();
        assert!(
            state.waiters.is_empty(),
            "a PI mutex cannot be destroyed with live scheduler waiters"
        );
    }
}

impl Default for PiMutexCore {
    fn default() -> Self {
        Self::new()
    }
}

/// Borrowed scheduler capability of one physical PI mutex.
#[derive(Clone, Copy, Debug)]
pub struct PiMutexRef<'lock> {
    core: &'lock PiMutexCore,
    id: PiMutexId,
}

/// Result of the atomic PI mutex fast acquisition path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PiMutexAcquire {
    /// The caller became the physical owner.
    Acquired,
    /// The caller must register in the scheduler-owned waiter tree.
    Contended,
}

impl<'lock> PiMutexRef<'lock> {
    /// Returns the stable generation-bearing lock identity.
    pub const fn id(self) -> PiMutexId {
        self.id
    }

    pub(crate) fn raw(self) -> PiMutexRaw {
        PiMutexRaw {
            core: NonNull::from(self.core),
            id: self.id,
        }
    }

    pub(crate) const fn core(self) -> &'lock PiMutexCore {
        self.core
    }

    pub(crate) fn lock_state(self) -> RawTicketGuard<'lock, PiMutexWaiters> {
        self.core.state.lock()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PiMutexRaw {
    core: NonNull<PiMutexCore>,
    id: PiMutexId,
}

impl PiMutexRaw {
    pub(crate) const fn id(self) -> PiMutexId {
        self.id
    }

    pub(crate) unsafe fn lock_state(self) -> RawTicketGuard<'static, PiMutexWaiters> {
        // SAFETY: every stored raw lock reference is lifetime-bound by the
        // public PiWaitToken and by the owning mutex guard. The embedded state
        // remains at this address until the final registration is removed.
        unsafe { self.core.as_ref() }.state.lock()
    }

    pub(crate) unsafe fn core(self) -> &'static PiMutexCore {
        // SAFETY: the raw capability is lifetime-bound by a wait token or
        // mutex transaction, both of which borrow the physical mutex core.
        unsafe { self.core.as_ref() }
    }
}

// SAFETY: PiMutexRaw is used only while a PiWaitToken or mutex borrow keeps the
// identity alive. The embedded raw ticket lock serializes mutable state.
unsafe impl Send for PiMutexRaw {}
unsafe impl Sync for PiMutexRaw {}

#[derive(Debug)]
pub(crate) struct PiMutexWaiters {
    pub(crate) waiters: PiWaitTree,
}

impl PiMutexWaiters {
    const fn new() -> Self {
        Self {
            waiters: PiWaitTree::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PiMutexOwnerSnapshot {
    word: u64,
    owner: Option<ThreadId>,
}

impl PiMutexOwnerSnapshot {
    pub(crate) const fn owner(self) -> Option<ThreadId> {
        self.owner
    }

    pub(crate) const fn is_unlocked(self) -> bool {
        self.word == 0
    }

    pub(crate) const fn is_ownerless(self) -> bool {
        self.word == OWNER_HAS_WAITERS
    }

    pub(crate) const fn has_waiters(self) -> bool {
        self.word & OWNER_HAS_WAITERS != 0
    }
}

fn owner_word(thread: ThreadId) -> Result<u64, TaskError> {
    let raw = thread.as_u64();
    if raw == 0 || raw & OWNER_HAS_WAITERS != 0 {
        return Err(TaskError::InvalidPiState);
    }
    Ok(raw)
}

fn owner_from_word(state: u64) -> Option<ThreadId> {
    let raw = state & OWNER_ID_MASK;
    (raw != 0).then(|| ThreadId::from_parts(raw as u32, (raw >> 32) as u32))
}

/// Token joining ax-sync's waiter grant with ax-task's parking transition.
///
/// The token retains the thread's preallocated wait state. Creating, granting,
/// cancelling, and dropping it never allocates memory.
#[must_use = "a PI wait token must be granted or explicitly cancelled"]
#[derive(Debug)]
pub struct PiWaitToken<'lock> {
    pub(crate) core: Arc<ThreadCore>,
    pub(crate) initial_owner: Option<Arc<ThreadCore>>,
    pub(crate) generation: u64,
    pub(crate) lock: PiMutexRaw,
    pub(crate) _lock_lifetime: PhantomData<&'lock PiMutexCore>,
}

impl PiWaitToken<'_> {
    /// Returns the generation-bearing identity of the registered waiter.
    pub fn thread_id(&self) -> ThreadId {
        self.core.id()
    }

    /// Returns whether ownership handoff has already selected this waiter.
    pub fn is_granted(&self) -> bool {
        self.core.pi_wait_state().is_granted(self.generation)
    }

    /// Returns whether an ownerless PI mutex selected this waiter to claim.
    ///
    /// Selection is only a wake-before-block handshake. The waiter does not
    /// own the mutex until its local owner-word claim and scheduler claim
    /// transaction both complete.
    pub fn is_selected(&self) -> bool {
        self.core.pi_wait_state().is_selected(self.generation)
    }

    /// Returns whether the scheduler selection and ownerless handoff are both
    /// published, so this waiter may enter the serialized claim transaction.
    pub fn can_claim(&self) -> bool {
        self.is_selected()
            && unsafe {
                // SAFETY: this token retains the physical mutex-core borrow.
                self.lock.core()
            }
            .owner_snapshot()
            .is_ownerless()
    }

    /// Returns whether this waiter is currently the lock's cached top waiter.
    pub fn is_top_waiter(&self) -> bool {
        self.core.pi_wait_state().is_top(self.generation)
    }

    /// Returns the owner observed by the wait-registration transaction.
    ///
    /// A waiter which joins an already ownerless claim window has no initial
    /// owner. A later handoff may also redirect an owned waiter, so mutex owner
    /// spinning must stop as soon as its local owner word no longer names this
    /// identity.
    pub fn initial_owner(&self) -> Option<ThreadId> {
        self.initial_owner.as_ref().map(|owner| owner.id())
    }

    /// Returns whether the initially observed owner still occupies a CPU.
    ///
    /// This is a progress hint equivalent to Linux `owner_on_cpu()`, not an
    /// ownership proof. The mutex owner word remains the serialization source
    /// of truth.
    pub fn initial_owner_is_on_cpu(&self) -> bool {
        self.initial_owner
            .as_ref()
            .is_some_and(|owner| owner.sched().scheduler_fence_cpu().is_some())
    }
}

#[derive(Debug)]
pub(crate) struct PiWaitState {
    generation: AtomicU64,
    top_generation: AtomicU64,
    selected_generation: AtomicU64,
    granted_generation: AtomicU64,
}

impl PiWaitState {
    pub(crate) const fn new() -> Self {
        Self {
            generation: AtomicU64::new(0),
            top_generation: AtomicU64::new(0),
            selected_generation: AtomicU64::new(0),
            granted_generation: AtomicU64::new(0),
        }
    }

    pub(crate) fn begin(&self) -> Result<u64, TaskError> {
        self.selected_generation.store(0, Ordering::Relaxed);
        self.top_generation.store(0, Ordering::Relaxed);
        self.granted_generation.store(0, Ordering::Relaxed);
        self.generation
            .try_update(Ordering::AcqRel, Ordering::Acquire, |generation| {
                generation.checked_add(1)
            })
            .map(|generation| generation + 1)
            .map_err(|_| TaskError::InvalidPiState)
    }

    pub(crate) fn select(&self, generation: u64) -> Result<(), TaskError> {
        if self.generation.load(Ordering::Acquire) != generation
            || self.granted_generation.load(Ordering::Acquire) == generation
        {
            return Err(TaskError::InvalidPiState);
        }
        self.selected_generation
            .store(generation, Ordering::Release);
        Ok(())
    }

    pub(crate) fn clear_selection(&self, generation: u64) {
        let _ = self.selected_generation.compare_exchange(
            generation,
            0,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    pub(crate) fn mark_top(&self, generation: u64) -> Result<(), TaskError> {
        if self.generation.load(Ordering::Acquire) != generation
            || self.granted_generation.load(Ordering::Acquire) == generation
        {
            return Err(TaskError::InvalidPiState);
        }
        self.top_generation.store(generation, Ordering::Release);
        Ok(())
    }

    pub(crate) fn clear_top(&self, generation: u64) {
        let _ = self.top_generation.compare_exchange(
            generation,
            0,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    pub(crate) fn grant(&self, generation: u64) -> Result<(), TaskError> {
        if self.generation.load(Ordering::Acquire) != generation {
            return Err(TaskError::InvalidPiState);
        }
        self.clear_selection(generation);
        self.clear_top(generation);
        self.granted_generation.store(generation, Ordering::Release);
        Ok(())
    }

    pub(crate) fn can_grant(&self, generation: u64) -> bool {
        self.generation.load(Ordering::Acquire) == generation
            && self.granted_generation.load(Ordering::Acquire) != generation
    }

    pub(crate) fn can_select(&self, generation: u64) -> bool {
        self.can_grant(generation) && self.selected_generation.load(Ordering::Acquire) != generation
    }

    fn is_granted(&self, generation: u64) -> bool {
        self.granted_generation.load(Ordering::Acquire) == generation
    }

    fn is_selected(&self, generation: u64) -> bool {
        self.selected_generation.load(Ordering::Acquire) == generation
    }

    fn is_top(&self, generation: u64) -> bool {
        self.top_generation.load(Ordering::Acquire) == generation
    }
}
