//! Linux-style per-lock PI waiter ownership and prepared local transactions.

use core::{fmt, marker::PhantomData};

use super::*;
use crate::{PiMutexRef, PiMutexWaiters, PiWaitStateError, ThreadWakeHandle};

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
        lock_state: &mut PiMutexWaiters,
        owner: Option<ThreadId>,
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
        self.publish_lock_top_change(owner, old_top, lock_state.waiters.first())
    }

    fn remove_lock_waiter(
        &mut self,
        lock_state: &mut PiMutexWaiters,
        owner: Option<ThreadId>,
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
        self.publish_lock_top_change(owner, old_top, lock_state.waiters.first())?;
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
            let _lock_state = unsafe {
                // SAFETY: blocked_on is lifetime-bound by the live wait token.
                registration.lock.lock_state()
            };
            return Ok(unsafe { registration.lock.core() }.owner_snapshot().owner());
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
        let owner = unsafe { registration.lock.core() }.owner_snapshot().owner();
        self.publish_lock_top_change(owner, old_top, lock_state.waiters.first())?;
        Ok(owner)
    }
}

/// Result of entering the PI mutex slow path.
#[must_use = "a registered PI waiter must be blocked, claimed, or cancelled"]
pub enum PiMutexLockResult<'lock> {
    /// A racing fast unlock let this caller acquire the mutex directly.
    Acquired,
    /// The caller is linked in the mutex-owned scheduler waiter tree.
    Waiting(PiWaitToken<'lock>),
}

impl fmt::Debug for PiMutexLockResult<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Acquired => formatter.write_str("Acquired"),
            Self::Waiting(token) => formatter
                .debug_tuple("Waiting")
                .field(&token.thread_id())
                .finish(),
        }
    }
}

impl TaskSystem {
    /// Registers one contender in the mutex-owned PI waiter tree.
    pub fn pi_mutex_lock_slow<'lock>(
        &self,
        lock: PiMutexRef<'lock>,
        waiter: ThreadId,
        sequence: u64,
    ) -> Result<PiMutexLockResult<'lock>, TaskError> {
        let mut state = self.state.lock();
        let lock_raw = lock.raw();
        let core = lock.core();
        let mut lock_state = lock.lock_state();
        if state.thread_record(waiter)?.sched.lock().lifecycle.state() == ThreadState::Exited {
            return Err(TaskError::InvalidPiWaitState(
                PiWaitStateError::ExitedParticipant,
            ));
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
        let generation = waiter_core.pi_wait_state().begin()?;

        loop {
            let snapshot = core.owner_snapshot();
            if snapshot.is_unlocked() {
                if !lock_state.waiters.is_empty() {
                    return Err(TaskError::InvalidPiWaitState(
                        PiWaitStateError::StaleSchedulerOwnership,
                    ));
                }
                if core.try_acquire_snapshot(snapshot, waiter)? {
                    return Ok(PiMutexLockResult::Acquired);
                }
                continue;
            }

            let owner = snapshot.owner();
            if owner == Some(waiter) {
                return Err(TaskError::InvalidPiWaitState(
                    PiWaitStateError::WaiterOwnsLock,
                ));
            }
            if snapshot.has_waiters() != !lock_state.waiters.is_empty()
                || snapshot.is_ownerless() && lock_state.waiters.is_empty()
            {
                return Err(TaskError::InvalidPiWaitState(
                    PiWaitStateError::StaleSchedulerOwnership,
                ));
            }
            if owner.is_none() && !snapshot.is_ownerless() {
                return Err(TaskError::InvalidPiWaitState(
                    PiWaitStateError::OwnerlessSelectionMissing,
                ));
            }
            if owner.is_some_and(|owner| {
                state.thread_record(owner).is_err()
                    || state.thread_record(owner).is_ok_and(|record| {
                        record.sched.lock().lifecycle.state() == ThreadState::Exited
                    })
            }) {
                return Err(TaskError::InvalidPiWaitState(
                    PiWaitStateError::ExitedParticipant,
                ));
            }
            if let Some(owner) = owner {
                state.ensure_pi_acyclic(waiter, owner, self.config.pi_chain_limit())?;
            }
            let becomes_top = lock_state.waiters.first().is_none_or(|top| key < top);
            let recompute = owner
                .filter(|_| becomes_top)
                .map(|owner| state.prepare_pi_recompute_chain(owner, self.config.pi_chain_limit()))
                .transpose()?;
            let initial_owner = owner
                .map(|owner| {
                    state
                        .thread_record(owner)
                        .map(|record| Arc::clone(&record.core))
                })
                .transpose()?;

            if !core.try_mark_waiters(snapshot) {
                continue;
            }
            state
                .insert_lock_waiter(
                    &mut lock_state,
                    owner,
                    waiter,
                    PiWaitRegistration {
                        lock: lock_raw,
                        key,
                        generation,
                    },
                )
                .expect("validated PI waiter insertion must remain infallible");
            if let Some(recompute) = recompute {
                state.apply_pi_recompute_chain(recompute, self.config.fair_slice_ns());
            }
            return Ok(PiMutexLockResult::Waiting(PiWaitToken {
                core: waiter_core,
                initial_owner,
                generation,
                lock: lock_raw,
                _lock_lifetime: PhantomData,
            }));
        }
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
            // SAFETY: token retains the mutex-core borrow for this call.
            token.lock.lock_state()
        };
        if token.is_selected() {
            return Err(TaskError::InvalidPiState);
        }
        let core = unsafe {
            // SAFETY: token retains the physical mutex-core borrow.
            token.lock.core()
        };
        let snapshot = core.owner_snapshot();
        let owner = snapshot.owner();
        if !snapshot.has_waiters() {
            return Err(TaskError::InvalidPiState);
        }
        let top_changes = lock_state.waiters.first() == Some(registration.key);
        let recompute = owner
            .filter(|_| top_changes)
            .map(|owner| state.prepare_pi_recompute_chain(owner, self.config.pi_chain_limit()))
            .transpose()?;
        state.remove_lock_waiter(&mut lock_state, owner, waiter)?;
        token.core.pi_wait_state().clear_selection(token.generation);
        if lock_state.waiters.is_empty() {
            let owner = owner.ok_or(TaskError::InvalidPiState)?;
            core.clear_waiters_bit(owner)?;
        }
        if let Some(recompute) = recompute {
            state.apply_pi_recompute_chain(recompute, self.config.fair_slice_ns());
        }
        Ok(())
    }

    /// Publishes an ownerless handoff and returns the selected wake target.
    pub fn pi_mutex_release(
        &self,
        lock: PiMutexRef<'_>,
        old_owner: ThreadId,
    ) -> Result<ThreadWakeHandle, TaskError> {
        let mut state = self.state.lock();
        let core = lock.core();
        let lock_state = lock.lock_state();
        let snapshot = core.owner_snapshot();
        if snapshot.owner() != Some(old_owner) || !snapshot.has_waiters() {
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
        let top = lock_state.waiters.first();
        state.replace_owner_lock_top(old_owner, top, None)?;
        state
            .thread_record(selected)?
            .core
            .pi_wait_state()
            .select(registration.generation)?;
        core.publish_ownerless();
        state.apply_pi_recompute_chain(old_recompute, self.config.fair_slice_ns());
        Ok(wake)
    }

    /// Claims an ownerless handoff selected for this waiter.
    pub fn pi_mutex_claim(&self, token: &PiWaitToken<'_>) -> Result<(), TaskError> {
        let mut state = self.state.lock();
        let claimant = token.thread_id();
        let lock = token.lock;
        let mut lock_state = unsafe {
            // SAFETY: the borrowed token keeps the physical mutex core live.
            lock.lock_state()
        };
        let core = unsafe {
            // SAFETY: the token lifetime is borrowed from this mutex core.
            lock.core()
        };
        if !core.owner_snapshot().is_ownerless() || !token.is_selected() {
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
        let registration = state.remove_lock_waiter(&mut lock_state, None, claimant)?;
        debug_assert_eq!(registration.generation, token.generation);
        core.publish_owner(claimant, !lock_state.waiters.is_empty());
        if let Some(top) = lock_state.waiters.first() {
            state.replace_owner_lock_top(claimant, None, Some(top))?;
        }
        let wait_state = &token.core.pi_wait_state();
        wait_state.clear_selection(token.generation);
        wait_state.grant(token.generation)?;
        state.apply_pi_recompute_chain(next_recompute, self.config.fair_slice_ns());
        Ok(())
    }
}
