//! Linux-style per-lock PI waiter ownership and prepared local transactions.

use core::{fmt, marker::PhantomData};

use super::*;
use crate::{
    PiLockIdentity, PiLockRef, PiLockWaitState, PiWaitOwner, PiWaitStateError, ThreadWakeHandle,
    lock::{PreemptTicketGuard, RawTicketGuard},
};

impl TaskSystemState {
    fn replace_owner_lock_top(
        &mut self,
        owner: ThreadId,
        old_top: Option<PiWaitKey>,
        new_top: Option<PiWaitKey>,
    ) -> Result<(), TaskError> {
        if old_top == new_top {
            return Ok(());
        }
        if let Some(old_top) = old_top {
            let removed = self
                .thread_record_mut(owner)?
                .pi_donors
                .remove(old_top)
                .ok_or(TaskError::InvalidPiState)?;
            let old_core = Arc::clone(&self.thread_record(old_top.thread)?.core);
            // SAFETY: removal detached the only owner-donor linkage belonging
            // to this waiter while the PI graph transaction remains exclusive.
            unsafe { old_core.pi_wait_nodes().return_owner_donor(removed) };
        }
        if let Some(new_top) = new_top {
            let new_core = Arc::clone(&self.thread_record(new_top.thread)?.core);
            let inserted = unsafe {
                // SAFETY: only the top waiter of one owned lock can consume
                // this thread's owner-donor linkage.
                new_core.pi_wait_nodes().take_owner_donor()
            };
            self.thread_record_mut(owner)?
                .pi_donors
                .insert(new_top, inserted);
        }
        let donating_locks = self.thread_record(owner)?.pi_donors.len();
        self.thread_record(owner)?.sched.lock().pi.donating_locks = donating_locks;
        Ok(())
    }

    fn publish_lock_top_change(
        &mut self,
        owner: Option<ThreadId>,
        old_top: Option<PiWaitKey>,
        new_top: Option<PiWaitKey>,
    ) -> Result<(), TaskError> {
        if old_top == new_top {
            return Ok(());
        }
        if let Some(old_top) = old_top {
            let old = self
                .thread_record(old_top.thread)?
                .blocked_on
                .ok_or(TaskError::InvalidPiState)?;
            self.thread_record(old_top.thread)?
                .core
                .pi_wait_state()
                .clear_top(old.generation);
        }
        if let Some(new_top) = new_top {
            let new = self
                .thread_record(new_top.thread)?
                .blocked_on
                .ok_or(TaskError::InvalidPiState)?;
            self.thread_record(new_top.thread)?
                .core
                .pi_wait_state()
                .mark_top(new.generation)?;
        }
        if let Some(owner) = owner {
            self.replace_owner_lock_top(owner, old_top, new_top)?;
        }
        Ok(())
    }

    fn insert_lock_waiter(
        &mut self,
        lock_state: &mut PiLockWaitState,
        waiter: ThreadId,
        registration: PiWaitRegistration,
    ) -> Result<(), TaskError> {
        if self.thread_record(waiter)?.blocked_on.is_some()
            || lock_state.waiters.contains(registration.key)
        {
            return Err(TaskError::InvalidPiState);
        }
        let old_top = lock_state.waiters.first();
        let core = Arc::clone(&self.thread_record(waiter)?.core);
        let node = unsafe {
            // SAFETY: a thread with no blocked_on registration cannot already
            // own a PI lock-waiter linkage.
            core.pi_wait_nodes().take_lock_waiter()
        };
        lock_state.waiters.insert(registration.key, node);
        self.thread_record_mut(waiter)?.blocked_on = Some(registration);
        self.publish_lock_top_change(lock_state.owner, old_top, lock_state.waiters.first())
    }

    fn remove_lock_waiter(
        &mut self,
        lock_state: &mut PiLockWaitState,
        waiter: ThreadId,
    ) -> Result<PiWaitRegistration, TaskError> {
        let registration = self
            .thread_record(waiter)?
            .blocked_on
            .ok_or(TaskError::InvalidPiState)?;
        let old_top = lock_state.waiters.first();
        let removed = lock_state
            .waiters
            .remove(registration.key)
            .ok_or(TaskError::InvalidPiState)?;
        self.publish_lock_top_change(lock_state.owner, old_top, lock_state.waiters.first())?;
        self.thread_record_mut(waiter)?.blocked_on = None;
        let core = Arc::clone(&self.thread_record(waiter)?.core);
        // SAFETY: the node is detached from the lock tree and the blocked_on
        // registration has been cleared in the same graph transaction.
        unsafe { core.pi_wait_nodes().return_lock_waiter(removed) };
        core.pi_wait_state().clear_top(registration.generation);
        Ok(registration)
    }

    pub(super) fn refresh_blocked_waiter_key(
        &mut self,
        waiter: ThreadId,
    ) -> Result<Option<ThreadId>, TaskError> {
        let Some(registration) = self.thread_record(waiter)?.blocked_on else {
            return Ok(None);
        };
        let urgency = {
            let sched = self.thread_record(waiter)?.sched.lock();
            sched
                .policy
                .effective_entity
                .scheduling_urgency(sched.policy.effective)
        };
        if urgency == registration.key.urgency {
            let lock_state = unsafe {
                // SAFETY: blocked_on is lifetime-bound by the live wait token.
                registration.lock.lock_state()
            };
            return Ok(lock_state.owner);
        }

        let mut lock_state = unsafe {
            // SAFETY: blocked_on is lifetime-bound by the live wait token.
            registration.lock.lock_state()
        };
        let old_top = lock_state.waiters.first();
        let node = lock_state
            .waiters
            .remove(registration.key)
            .ok_or(TaskError::InvalidPiState)?;
        let new_key = PiWaitKey::new(urgency, registration.key.sequence, waiter);
        lock_state.waiters.insert(new_key, node);
        self.thread_record_mut(waiter)?
            .blocked_on
            .as_mut()
            .ok_or(TaskError::InvalidPiState)?
            .key = new_key;
        self.publish_lock_top_change(lock_state.owner, old_top, lock_state.waiters.first())?;
        Ok(lock_state.owner)
    }
}

/// Prepared scheduler transaction for publishing one local PI waiter.
#[must_use = "a prepared PI waiter must be committed after local publication or dropped"]
pub struct PiWaitStart<'system, 'lock> {
    state: PreemptTicketGuard<'system, TaskSystemState>,
    lock_state: RawTicketGuard<'lock, PiLockWaitState>,
    fair_slice_ns: u64,
    lock: PiLockRaw,
    waiter: ThreadId,
    waiter_core: Arc<ThreadCore>,
    initial_owner: Option<Arc<ThreadCore>>,
    generation: u64,
    key: PiWaitKey,
    owner: Option<ThreadId>,
    initialize_owner: bool,
    recompute: Option<PiRecomputeProof>,
    _lock_lifetime: PhantomData<&'lock PiLockIdentity>,
}

impl fmt::Debug for PiWaitStart<'_, '_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PiWaitStart")
            .field("lock", &self.lock.id())
            .field("waiter", &self.waiter)
            .field("generation", &self.generation)
            .field("key", &self.key)
            .field("owner", &self.owner)
            .finish_non_exhaustive()
    }
}

impl<'lock> PiWaitStart<'_, 'lock> {
    /// Publishes the prevalidated scheduler registration.
    ///
    /// # Safety
    ///
    /// The owning mutex must hold its slow-path gate and have inserted the
    /// matching pinned waiter into its local metadata queue. The gate must stay
    /// held through this commit, and the waiter must remain present until the
    /// returned token is cancelled or granted.
    pub unsafe fn commit_after_local_registration(mut self) -> PiWaitToken<'lock> {
        if self.initialize_owner {
            self.lock_state.owner = self.owner;
        }
        self.state
            .insert_lock_waiter(
                &mut self.lock_state,
                self.waiter,
                PiWaitRegistration {
                    lock: self.lock,
                    key: self.key,
                    generation: self.generation,
                },
            )
            .expect("prepared PI waiter insertion must remain valid");
        if let Some(recompute) = self.recompute {
            self.state
                .apply_pi_recompute_chain(recompute, self.fair_slice_ns);
        }
        PiWaitToken {
            core: self.waiter_core,
            initial_owner: self.initial_owner,
            generation: self.generation,
            lock: self.lock,
            _lock_lifetime: PhantomData,
        }
    }
}

/// Prepared scheduler half of releasing one contended PI mutex.
#[must_use = "a prepared PI release must be committed after local publication or dropped"]
pub struct PiMutexRelease<'system, 'lock> {
    state: PreemptTicketGuard<'system, TaskSystemState>,
    lock_state: RawTicketGuard<'lock, PiLockWaitState>,
    fair_slice_ns: u64,
    old_owner: ThreadId,
    selected: ThreadId,
    selected_generation: u64,
    wake: ThreadWakeHandle,
    old_recompute: PiRecomputeProof,
    _lock_lifetime: PhantomData<&'lock PiLockIdentity>,
}

impl fmt::Debug for PiMutexRelease<'_, '_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PiMutexRelease")
            .field("old_owner", &self.old_owner)
            .field("selected", &self.selected)
            .finish_non_exhaustive()
    }
}

impl PiMutexRelease<'_, '_> {
    /// Returns the waiter selected by the scheduler's cached lock top.
    pub const fn selected(&self) -> ThreadId {
        self.selected
    }

    /// Removes this lock's top donation, publishes ownerless selection, and
    /// returns the targeted wake capability.
    ///
    /// # Safety
    ///
    /// The mutex must hold its slow-path gate and have published its ownerless
    /// owner word. The gate must stay held through this scheduler selection.
    pub unsafe fn commit_after_local_release(mut self) -> ThreadWakeHandle {
        let top = self.lock_state.waiters.first();
        self.state
            .replace_owner_lock_top(self.old_owner, top, None)
            .expect("prepared PI release top must remain linked");
        self.lock_state.owner = None;
        self.lock_state.selected = Some(self.selected);
        self.state
            .thread_record(self.selected)
            .expect("prepared PI selected waiter must remain live")
            .core
            .pi_wait_state()
            .select(self.selected_generation)
            .expect("prepared PI selection generation must remain current");
        self.state
            .apply_pi_recompute_chain(self.old_recompute, self.fair_slice_ns);
        self.wake
    }
}

/// Prepared scheduler half of claiming an ownerless PI mutex.
#[must_use = "a prepared PI claim must be committed after local ownership publication or dropped"]
pub struct PiMutexClaim<'system, 'lock> {
    state: PreemptTicketGuard<'system, TaskSystemState>,
    lock_state: RawTicketGuard<'lock, PiLockWaitState>,
    fair_slice_ns: u64,
    claimant: ThreadId,
    generation: u64,
    next_recompute: PiRecomputeProof,
    _lock_lifetime: PhantomData<&'lock PiLockIdentity>,
}

impl fmt::Debug for PiMutexClaim<'_, '_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PiMutexClaim")
            .field("claimant", &self.claimant)
            .finish_non_exhaustive()
    }
}

impl PiMutexClaim<'_, '_> {
    /// Attaches the remaining lock top to the claimant and grants ownership.
    ///
    /// # Safety
    ///
    /// The mutex must hold its slow-path gate, have removed `claimant` from
    /// local waiter metadata, and have published it as physical owner. The gate
    /// must stay held through this scheduler claim commit.
    pub unsafe fn commit_after_local_claim(mut self) {
        let registration = self
            .state
            .remove_lock_waiter(&mut self.lock_state, self.claimant)
            .expect("prepared PI claimant must remain in the lock waiter tree");
        debug_assert_eq!(registration.generation, self.generation);
        self.lock_state.selected = None;
        if let Some(top) = self.lock_state.waiters.first() {
            self.lock_state.owner = Some(self.claimant);
            self.state
                .replace_owner_lock_top(self.claimant, None, Some(top))
                .expect("remaining PI lock top must attach to claimant");
        } else {
            self.lock_state.owner = None;
        }
        let wait_state = self
            .state
            .thread_record(self.claimant)
            .expect("prepared PI claimant must remain live")
            .core
            .pi_wait_state();
        wait_state.clear_selection(self.generation);
        wait_state
            .grant(self.generation)
            .expect("prepared PI claimant generation must remain current");
        self.state
            .apply_pi_recompute_chain(self.next_recompute, self.fair_slice_ns);
    }
}

impl TaskSystem {
    /// Prepares a lock-local waiter insertion without publishing a graph edge.
    pub fn prepare_pi_wait_start<'lock>(
        &self,
        lock: PiLockRef<'lock>,
        waiter: ThreadId,
        owner: PiWaitOwner,
        sequence: u64,
    ) -> Result<PiWaitStart<'_, 'lock>, TaskError> {
        let state = self.state.lock();
        let lock_raw = lock.raw();
        let lock_state = lock.lock_state();
        let (owner, initialize_owner) = match owner {
            PiWaitOwner::Owned(owner) => {
                if waiter == owner {
                    return Err(TaskError::InvalidPiWaitState(
                        PiWaitStateError::WaiterOwnsLock,
                    ));
                }
                if lock_state.waiters.is_empty() {
                    if lock_state.owner.is_some() || lock_state.selected.is_some() {
                        return Err(TaskError::InvalidPiWaitState(
                            PiWaitStateError::StaleSchedulerOwnership,
                        ));
                    }
                    (Some(owner), true)
                } else if lock_state.owner == Some(owner) && lock_state.selected.is_none() {
                    (Some(owner), false)
                } else {
                    return Err(TaskError::InvalidPiWaitState(
                        PiWaitStateError::PhysicalOwnerMismatch,
                    ));
                }
            }
            PiWaitOwner::Ownerless => {
                if lock_state.owner.is_some()
                    || lock_state.selected.is_none()
                    || lock_state.waiters.is_empty()
                {
                    return Err(TaskError::InvalidPiWaitState(
                        PiWaitStateError::OwnerlessSelectionMissing,
                    ));
                }
                (None, false)
            }
        };
        if state.thread_record(waiter)?.sched.lock().lifecycle.state() == ThreadState::Exited
            || owner.is_some_and(|owner| {
                state.thread_record(owner).is_ok_and(|record| {
                    record.sched.lock().lifecycle.state() == ThreadState::Exited
                })
            })
        {
            return Err(TaskError::InvalidPiWaitState(
                PiWaitStateError::ExitedParticipant,
            ));
        }
        if let Some(owner) = owner {
            state.ensure_pi_acyclic(waiter, owner, self.config.pi_chain_limit())?;
        }
        if state.thread_record(waiter)?.blocked_on.is_some() {
            return Err(TaskError::InvalidPiWaitState(
                PiWaitStateError::WaiterAlreadyBlocked,
            ));
        }
        state.validate_pi_donor(waiter)?;
        let waiter_core = Arc::clone(&state.thread_record(waiter)?.core);
        let urgency = {
            let sched = state.thread_record(waiter)?.sched.lock();
            sched
                .policy
                .effective_entity
                .scheduling_urgency(sched.policy.effective)
        };
        let key = PiWaitKey::new(urgency, sequence, waiter);
        let becomes_top = lock_state.waiters.first().is_none_or(|top| key < top);
        let recompute = owner
            .filter(|_| becomes_top)
            .map(|owner| state.prepare_pi_recompute_chain(owner, self.config.pi_chain_limit()))
            .transpose()?;
        let initial_owner = match owner {
            Some(owner) => Some(Arc::clone(&state.thread_record(owner)?.core)),
            None => None,
        };
        let generation = waiter_core.pi_wait_state().begin()?;
        Ok(PiWaitStart {
            state,
            lock_state,
            fair_slice_ns: self.config.fair_slice_ns(),
            lock: lock_raw,
            waiter,
            waiter_core,
            initial_owner,
            generation,
            key,
            owner,
            initialize_owner,
            recompute,
            _lock_lifetime: PhantomData,
        })
    }

    /// Cancels a committed waiter which has not been selected for claim.
    pub fn pi_wait_cancel(&self, token: PiWaitToken<'_>) -> Result<(), TaskError> {
        let mut state = self.state.lock();
        let waiter = token.thread_id();
        let registration = state
            .thread_record(waiter)?
            .blocked_on
            .filter(|registration| registration.generation == token.generation)
            .ok_or(TaskError::InvalidPiState)?;
        if registration.lock != token.lock {
            return Err(TaskError::InvalidPiState);
        }
        let mut lock_state = unsafe {
            // SAFETY: token retains the lock-identity borrow for this call.
            token.lock.lock_state()
        };
        if lock_state.selected == Some(waiter) {
            return Err(TaskError::InvalidPiState);
        }
        let owner = lock_state.owner;
        let top_changes = lock_state.waiters.first() == Some(registration.key);
        let recompute = owner
            .filter(|_| top_changes)
            .map(|owner| state.prepare_pi_recompute_chain(owner, self.config.pi_chain_limit()))
            .transpose()?;
        state.remove_lock_waiter(&mut lock_state, waiter)?;
        token.core.pi_wait_state().clear_selection(token.generation);
        if lock_state.waiters.is_empty() {
            lock_state.owner = None;
        }
        if let Some(recompute) = recompute {
            state.apply_pi_recompute_chain(recompute, self.config.fair_slice_ns());
        }
        Ok(())
    }

    /// Prepares release using the lock tree's cached top waiter.
    pub fn prepare_pi_mutex_release<'lock>(
        &self,
        lock: PiLockRef<'lock>,
        old_owner: ThreadId,
    ) -> Result<PiMutexRelease<'_, 'lock>, TaskError> {
        let state = self.state.lock();
        let lock_state = lock.lock_state();
        if lock_state.owner != Some(old_owner) || lock_state.selected.is_some() {
            return Err(TaskError::InvalidPiState);
        }
        let selected_key = lock_state
            .waiters
            .first()
            .ok_or(TaskError::InvalidPiState)?;
        let selected = selected_key.thread;
        let registration = state
            .thread_record(selected)?
            .blocked_on
            .filter(|registration| registration.lock.id() == lock.id())
            .ok_or(TaskError::InvalidPiState)?;
        if !state
            .thread_record(selected)?
            .core
            .pi_wait_state()
            .can_select(registration.generation)
            || !state
                .thread_record(old_owner)?
                .pi_donors
                .contains(selected_key)
        {
            return Err(TaskError::InvalidPiState);
        }
        let wake = ThreadWakeHandle::from_core(Arc::clone(&state.thread_record(selected)?.core));
        let old_recompute =
            state.prepare_pi_recompute_chain(old_owner, self.config.pi_chain_limit())?;
        Ok(PiMutexRelease {
            state,
            lock_state,
            fair_slice_ns: self.config.fair_slice_ns(),
            old_owner,
            selected,
            selected_generation: registration.generation,
            wake,
            old_recompute,
            _lock_lifetime: PhantomData,
        })
    }

    /// Prepares the selected waiter to claim an ownerless lock.
    pub fn prepare_pi_mutex_claim<'lock>(
        &self,
        token: &PiWaitToken<'lock>,
    ) -> Result<PiMutexClaim<'_, 'lock>, TaskError> {
        let state = self.state.lock();
        let claimant = token.thread_id();
        let lock = token.lock;
        let lock_state = unsafe {
            // SAFETY: the borrowed token keeps the physical PI lock identity
            // live for the returned transaction's complete lifetime.
            lock.lock_state()
        };
        if lock_state.owner.is_some() || lock_state.selected != Some(claimant) {
            return Err(TaskError::InvalidPiState);
        }
        let registration = state
            .thread_record(claimant)?
            .blocked_on
            .filter(|registration| {
                registration.lock == lock && registration.generation == token.generation
            })
            .ok_or(TaskError::InvalidPiState)?;
        if !lock_state.waiters.contains(registration.key)
            || !state
                .thread_record(claimant)?
                .core
                .pi_wait_state()
                .can_grant(registration.generation)
        {
            return Err(TaskError::InvalidPiState);
        }
        let next_recompute = state.prepare_pi_recompute_after_claim(claimant, registration)?;
        Ok(PiMutexClaim {
            state,
            lock_state,
            fair_slice_ns: self.config.fair_slice_ns(),
            claimant,
            generation: registration.generation,
            next_recompute,
            _lock_lifetime: PhantomData,
        })
    }
}
