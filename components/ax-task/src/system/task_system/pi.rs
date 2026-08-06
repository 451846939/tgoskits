//! Linux-style per-lock PI waiter ownership and bounded chain propagation.

use core::{fmt, marker::PhantomData};

use super::*;
use crate::{PiMutexRef, PiMutexWaiters, PiWaitStateError, lock::PreemptScope};

const PI_RELEASE_WAKE_INVARIANT: u32 = 0x5049_574b;

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

#[derive(Clone, Copy)]
enum PiRqFollowup {
    RemoteReschedule,
    SchedulerWork,
}

struct PiWaiterRefresh {
    owner: Option<ThreadId>,
    changed: bool,
}

#[derive(Clone, Copy)]
struct PiWaiterRekey {
    waiter: ThreadId,
    registration: PiWaitRegistration,
    new_key: PiWaitKey,
}

impl TaskSystem {
    /// Acquires a stable task reference without retaining the registry lock.
    ///
    /// This is the local `get_task_struct()` operation used by the PI chain
    /// walk. All PI graph state is protected by the returned task's scheduler
    /// lock, never by the registry lock used for this lookup.
    fn pi_thread_core(&self, thread: ThreadId) -> Result<Arc<ThreadCore>, TaskError> {
        let state = self.state.lock();
        Ok(Arc::clone(&state.thread_record(thread)?.core))
    }

    fn pi_donation(&self, core: &Arc<ThreadCore>) -> Result<PiDonation, TaskError> {
        let (policy, root) = {
            let sched = core.sched().lock();
            (
                core.effective_policy_snapshot(),
                sched.pi.donor.unwrap_or(core.id()),
            )
        };
        let root_core = if root == core.id() {
            Arc::clone(core)
        } else {
            self.pi_thread_core(root)?
        };
        Ok(PiDonation::new(
            policy,
            root,
            core.effective_scheduling_urgency(),
            core,
            &root_core,
        ))
    }

    pub(super) fn resolved_pi_schedule_update(
        &self,
        base: SchedulePolicy,
        base_entity: SchedulingEntity,
        donor: Option<(PiWaitKey, PiDonation)>,
        generation: u64,
    ) -> Result<PiScheduleUpdate, TaskError> {
        let mut policy = base;
        let mut effective_urgency = base_entity.scheduling_urgency(base);
        let mut pi_donor = None;
        let mut deadline_donor = None;
        if let Some((_top, donor)) = donor.as_ref()
            && donor.boost_urgency < effective_urgency
            && let Some(inherited) = pi_inherited_policy(base, donor.policy)
        {
            policy = inherited;
            effective_urgency = donor.boost_urgency;
            pi_donor = Some(donor.root);
            deadline_donor =
                matches!(donor.policy, SchedulePolicy::Deadline(_)).then_some(donor.root);
        }
        let _ = effective_urgency;
        let deadline_donor_core = deadline_donor.map(|donor_id| {
            let (_, donor) = donor
                .as_ref()
                .filter(|(_, donor)| donor.root == donor_id)
                .expect("resolved Deadline donor must retain its task reference");
            donor.root_core.clone()
        });
        let deadline_donor_server = deadline_donor_core
            .as_ref()
            .map(|core| {
                core.upgrade()
                    .ok_or(TaskError::InvalidPiState)
                    .map(|core| core.sched().deadline_server())
            })
            .transpose()?;
        Ok(PiScheduleUpdate {
            policy,
            donor: pi_donor,
            deadline_donor,
            deadline_donor_core,
            deadline_donor_server,
            generation,
        })
    }

    /// Applies one effective-priority change under `p->pi_lock + rq->lock`.
    ///
    /// This is the ax-task equivalent of Linux `rt_mutex_setprio()`. The task
    /// is detached from its class at most once, the owner rq clock is sampled
    /// once, and the effective entity plus all IRQ-visible dispatch metadata
    /// are committed before the rq publication becomes visible.
    fn apply_pi_schedule_update_in_rq(
        &self,
        core: &Arc<ThreadCore>,
        sched: &mut ThreadSchedState,
        update: PiScheduleUpdate,
        transaction: &mut OwnerRqTxn<'_>,
    ) -> PiRqFollowup {
        let owner = sched
            .placement
            .assigned_cpu()
            .expect("PI target must retain task_cpu()");
        if transaction.owner() != owner {
            task_runtime::fatal_invariant(0x5049_1206, core.id().as_u64() as usize);
        }
        let running = sched.placement.execution_cpu() == Some(owner);
        let queued = sched.placement.queued_cpu() == Some(owner) && !running;
        let owner_now_ns = transaction.clock().wall().as_nanos();
        let source_fair = sched
            .policy
            .active_option()
            .and_then(|active| active.base_entity().fair())
            .or_else(|| {
                transaction
                    .base_scheduling_entity(core.id())
                    .and_then(|entity| entity.fair())
            });
        let fair_placement = match (source_fair, update.policy) {
            (Some(source), SchedulePolicy::Fair { mode, .. }) => Some(FairPolicyPlacement {
                source_virtual_time: transaction.virtual_time_for_mode(source.mode()),
                destination_virtual_time: transaction.virtual_time_for_mode(mode),
            }),
            _ => None,
        };
        if running {
            if transaction.current_thread() != Some(core.id()) {
                task_runtime::fatal_invariant(0x5049_1201, core.id().as_u64() as usize);
            }
            let active = transaction.detach_current_schedule(core.id());
            let active =
                apply_pi_schedule_update(sched, active, update, owner_now_ns, fair_placement)
                    .unwrap_or_else(|_| {
                        task_runtime::fatal_invariant(0x5049_1207, core.id().as_u64() as usize)
                    });
            let policy = active.policy();
            let entity = active.entity().clone();
            let rt_quota_exempt = sched.is_pi_boosted_rt_owner_for(policy);
            let metadata = sched.rq_task_metadata().unwrap_or_else(|_| {
                task_runtime::fatal_invariant(0x5049_1208, core.id().as_u64() as usize)
            });
            transaction.install_current_schedule(
                core.id(),
                active,
                Arc::clone(core),
                rt_quota_exempt,
                sched.affinity.affinity.is_migration_capable(),
                metadata.clone(),
            );
            transaction
                .current_mut()
                .expect("running PI target must remain current")
                .refresh_scheduler_metadata(metadata, rt_quota_exempt);
            core.publish_effective_schedule(policy, &entity);
            return PiRqFollowup::RemoteReschedule;
        }
        if queued {
            let current_fair = transaction
                .current_scheduling_entity()
                .and_then(|entity| entity.fair());
            let active = transaction.reclassify_task(core.id()).into_active();
            let active =
                apply_pi_schedule_update(sched, active, update, owner_now_ns, fair_placement)
                    .unwrap_or_else(|_| {
                        task_runtime::fatal_invariant(0x5049_1209, core.id().as_u64() as usize)
                    });
            let policy = active.policy();
            let entity = active.entity().clone();
            let rt_quota_exempt = sched.is_pi_boosted_rt_owner_for(policy);
            let metadata = sched.rq_task_metadata().unwrap_or_else(|_| {
                task_runtime::fatal_invariant(0x5049_120a, core.id().as_u64() as usize)
            });
            transaction.enqueue_task(
                QueuedThread::new(
                    core.id(),
                    active,
                    Arc::clone(core),
                    rt_quota_exempt,
                    sched.affinity.affinity.is_migration_capable(),
                    metadata,
                ),
                EnqueueReason::PolicyChanged,
                current_fair,
            );
            core.publish_effective_schedule(policy, &entity);
            return PiRqFollowup::RemoteReschedule;
        }
        let active = sched.policy.take_active();
        let active = apply_pi_schedule_update(sched, active, update, owner_now_ns, fair_placement)
            .unwrap_or_else(|_| {
                task_runtime::fatal_invariant(0x5049_120b, core.id().as_u64() as usize)
            });
        core.publish_effective_schedule(active.policy(), active.entity());
        sched.policy.install_active(active);
        PiRqFollowup::SchedulerWork
    }

    /// Recomputes `pi_top_task` and the effective class while holding the task
    /// PI lock, then commits the class change under the same owner-rq lock.
    ///
    /// The donor snapshot is cloned into `pi_waiters`, so this path never takes
    /// another task lock. This is the direct analogue of Linux
    /// `rt_mutex_adjust_prio()` -> `rt_mutex_setprio()`.
    fn recompute_pi_owner_locked(
        &self,
        core: &Arc<ThreadCore>,
        sched: &mut ThreadSchedState,
        donor: Option<(PiWaitKey, PiDonation)>,
    ) -> Result<bool, TaskError> {
        let owner = sched
            .placement
            .assigned_cpu()
            .ok_or(TaskError::InvalidPiState)?;
        let remote = self
            .cpu_remotes
            .get(owner.as_usize())
            .ok_or(TaskError::InvalidPiState)?;
        if !remote.is_online() {
            return Err(TaskError::CpuOffline(owner.as_u32()));
        }
        let mut transaction = OwnerRqTxn::begin(self, remote);
        if transaction.current().is_some() {
            let _settled = transaction.settle_current(0);
        }
        let base_entity = sched
            .policy
            .active_option()
            .map(|active| active.base_entity().clone())
            .or_else(|| transaction.base_scheduling_entity(core.id()));
        let Some(base_entity) = base_entity else {
            transaction.commit();
            return Err(TaskError::InvalidPiState);
        };
        let Some(generation) = sched.policy.dispatch_generation.checked_add(1) else {
            transaction.commit();
            return Err(TaskError::InvalidConfiguration);
        };
        let update = match self.resolved_pi_schedule_update(
            sched.policy.base,
            base_entity,
            donor,
            generation,
        ) {
            Ok(update) => update,
            Err(error) => {
                transaction.commit();
                return Err(error);
            }
        };
        let changed = core.effective_policy_snapshot() != update.policy
            || sched.pi.donor != update.donor
            || sched.pi.deadline_donor != update.deadline_donor;
        let followup = if changed {
            sched.policy.dispatch_generation = generation;
            Some(self.apply_pi_schedule_update_in_rq(core, sched, update, &mut transaction))
        } else {
            None
        };
        transaction.commit();
        match followup {
            Some(PiRqFollowup::RemoteReschedule) => remote.request_remote_reschedule(),
            Some(PiRqFollowup::SchedulerWork) => remote.request_scheduler_work(),
            None => {}
        }
        Ok(changed)
    }

    /// Replaces the one waiter-tree top contributed by a physical mutex.
    ///
    /// The caller owns that mutex's wait lock. This method then acquires only
    /// the owner task's PI lock, exactly matching Linux's wait-lock -> pi-lock
    /// order for `rt_mutex_enqueue_pi()`/`rt_mutex_dequeue_pi()`.
    fn replace_owner_lock_top(
        &self,
        owner: ThreadId,
        old_top: Option<(PiWaitKey, PiDonation)>,
        new_top: Option<(PiWaitKey, PiDonation)>,
    ) -> Result<(), TaskError> {
        let owner_core = self.pi_thread_core(owner)?;
        let old_core = old_top
            .as_ref()
            .map(|(_, donation)| donation.waiter_core().ok_or(TaskError::InvalidPiState))
            .transpose()?;
        let new_core = new_top
            .as_ref()
            .map(|(_, donation)| donation.waiter_core().ok_or(TaskError::InvalidPiState))
            .transpose()?;
        let mut owner_sched = owner_core.sched().lock();
        if owner_sched.lifecycle.state() == ThreadState::Exited {
            return Err(TaskError::InvalidPiWaitState(
                PiWaitStateError::ExitedParticipant,
            ));
        }
        if let (Some((old_top, _)), Some((new_top, donation))) =
            (old_top.as_ref(), new_top.as_ref())
            && *old_top == *new_top
            && owner_sched
                .pi
                .donors
                .donation(*old_top)
                .is_some_and(|current| current.same_source(donation))
        {
            return Ok(());
        }
        // Linux validates the complete rt_mutex_setprio() transaction before
        // changing p->pi_waiters. A generation overflow is a typed policy
        // failure, not a reason to strand the physical waiter tree on a
        // donor which the owner rq could not publish.
        owner_sched
            .policy
            .dispatch_generation
            .checked_add(1)
            .ok_or(TaskError::InvalidConfiguration)?;
        let old_key = old_top.as_ref().map(|(key, _)| *key);
        let remaining_top = owner_sched.pi.donors.first_entry_excluding(old_key);
        let prospective_top = match (remaining_top, new_top.as_ref()) {
            (Some(current), Some(candidate)) if candidate.0 < current.0 => Some(candidate.clone()),
            (Some(current), _) => Some(current),
            (None, Some(candidate)) => Some(candidate.clone()),
            (None, None) => None,
        };
        if let Some((old_top, _)) = old_top.as_ref() {
            let removed = owner_sched
                .pi
                .donors
                .remove(*old_top)
                .ok_or(TaskError::InvalidPiState)?;
            // SAFETY: the mutex wait lock and owner PI lock detached the only
            // owner-tree linkage which can use this preallocated node.
            unsafe {
                old_core
                    .as_ref()
                    .expect("old PI top must retain its task")
                    .pi_wait_nodes()
                    .return_owner_donor(removed)
            };
        }
        if let Some((new_top, donation)) = new_top.as_ref() {
            let new_core = new_core.as_ref().expect("new PI top must retain its task");
            let inserted = unsafe {
                // SAFETY: one blocked waiter is the top waiter of at most one
                // mutex and therefore can own one owner-tree linkage.
                new_core.pi_wait_nodes().take_owner_donor()
            };
            owner_sched
                .pi
                .donors
                .insert(*new_top, donation.clone(), inserted);
        }
        if let Err(error) =
            self.recompute_pi_owner_locked(&owner_core, &mut owner_sched, prospective_top)
        {
            if let Some((new_top, _)) = new_top.as_ref() {
                let removed = owner_sched
                    .pi
                    .donors
                    .remove(*new_top)
                    .expect("failed PI owner update must retain the proposed donor");
                // SAFETY: the failed transaction removed the only owner-tree
                // linkage before returning it to the waiter's storage.
                unsafe {
                    new_core
                        .as_ref()
                        .expect("new PI top must retain its task")
                        .pi_wait_nodes()
                        .return_owner_donor(removed)
                };
            }
            if let Some((old_top, donation)) = old_top.as_ref() {
                let old_core = old_core.as_ref().expect("old PI top must retain its task");
                let restored = unsafe {
                    // SAFETY: the failed transaction returned this task's
                    // owner linkage above and no other owner can consume it
                    // while the physical mutex wait lock is held.
                    old_core.pi_wait_nodes().take_owner_donor()
                };
                owner_sched
                    .pi
                    .donors
                    .insert(*old_top, donation.clone(), restored);
            }
            return Err(error);
        }
        Ok(())
    }

    /// Publishes a physical mutex's cached-top change while its wait lock is held.
    fn publish_lock_top_change(
        &self,
        owner: Option<ThreadId>,
        old_top: Option<(PiWaitKey, PiDonation)>,
        new_top: Option<(PiWaitKey, PiDonation)>,
        rekey: Option<PiWaiterRekey>,
    ) -> Result<(), TaskError> {
        let old_key = old_top.as_ref().map(|(key, _)| *key);
        let new_key = new_top.as_ref().map(|(key, _)| *key);
        let marker = |top: &Option<(PiWaitKey, PiDonation)>| {
            let Some((key, donation)) = top.as_ref() else {
                return Ok(None);
            };
            let core = donation.waiter_core().ok_or(TaskError::InvalidPiState)?;
            if let Some(rekey) = rekey
                && core.id() == rekey.waiter
                && (*key == rekey.registration.key || *key == rekey.new_key)
            {
                return Ok(Some((core, rekey.registration.generation)));
            }
            let registration = core
                .sched()
                .lock()
                .pi
                .blocked_on
                .ok_or(TaskError::InvalidPiState)?;
            if registration.key != *key {
                return Err(TaskError::InvalidPiState);
            }
            Ok(Some((core, registration.generation)))
        };
        let (old_marker, new_marker) = if old_key != new_key {
            let old_marker = marker(&old_top)?;
            let new_marker = marker(&new_top)?;
            if let Some((core, generation)) = new_marker.as_ref()
                && !core.pi_wait_state().can_grant(*generation)
            {
                return Err(TaskError::InvalidPiState);
            }
            (old_marker, new_marker)
        } else {
            (None, None)
        };
        if let Some(owner) = owner {
            self.replace_owner_lock_top(owner, old_top, new_top)?;
        }
        if let Some((core, generation)) = old_marker {
            core.pi_wait_state().clear_top(generation);
        }
        if let Some((core, generation)) = new_marker {
            core.pi_wait_state()
                .mark_top(generation)
                .unwrap_or_else(|_| {
                    task_runtime::fatal_invariant(0x5049_1204, core.id().as_u64() as usize)
                });
        }
        Ok(())
    }

    fn insert_lock_waiter(
        &self,
        lock_state: &mut PiMutexWaiters,
        owner: Option<ThreadId>,
        waiter_core: &Arc<ThreadCore>,
        registration: PiWaitRegistration,
        donation: PiDonation,
    ) -> Result<(), TaskError> {
        let old_top = lock_state.waiters.first_entry();
        {
            let mut waiter_sched = waiter_core.sched().lock();
            if waiter_sched.pi.blocked_on.is_some() || lock_state.waiters.contains(registration.key)
            {
                return Err(TaskError::InvalidPiState);
            }
            let node = unsafe {
                // SAFETY: `blocked_on == None` proves this task cannot already
                // own a physical-lock waiter linkage.
                waiter_core.pi_wait_nodes().take_lock_waiter()
            };
            lock_state.waiters.insert(registration.key, donation, node);
            waiter_sched.pi.blocked_on = Some(registration);
        }
        if let Err(error) =
            self.publish_lock_top_change(owner, old_top, lock_state.waiters.first_entry(), None)
        {
            let removed = lock_state
                .waiters
                .remove(registration.key)
                .ok_or(TaskError::InvalidPiState)?;
            let mut waiter_sched = waiter_core.sched().lock();
            if waiter_sched.pi.blocked_on != Some(registration) {
                return Err(TaskError::InvalidPiState);
            }
            waiter_sched.pi.blocked_on = None;
            drop(waiter_sched);
            // SAFETY: wait_lock detached the failed insertion and the task PI
            // lock removed its only registration before storage is returned.
            unsafe { waiter_core.pi_wait_nodes().return_lock_waiter(removed) };
            return Err(error);
        }
        Ok(())
    }

    fn remove_lock_waiter(
        &self,
        lock_state: &mut PiMutexWaiters,
        owner: Option<ThreadId>,
        waiter_core: &Arc<ThreadCore>,
        generation: u64,
    ) -> Result<PiWaitRegistration, TaskError> {
        let old_top = lock_state.waiters.first_entry();
        let (registration, removed, donation) = {
            let waiter_sched = waiter_core.sched().lock();
            let registration = waiter_sched
                .pi
                .blocked_on
                .filter(|registration| registration.generation == generation)
                .ok_or(TaskError::InvalidPiState)?;
            let donation = lock_state
                .waiters
                .donation(registration.key)
                .ok_or(TaskError::InvalidPiState)?;
            let removed = lock_state
                .waiters
                .remove(registration.key)
                .ok_or(TaskError::InvalidPiState)?;
            (registration, removed, donation)
        };
        if let Err(error) = self.publish_lock_top_change(
            owner,
            old_top.clone(),
            lock_state.waiters.first_entry(),
            None,
        ) {
            lock_state
                .waiters
                .insert(registration.key, donation, removed);
            return Err(error);
        }
        {
            let mut waiter_sched = waiter_core.sched().lock();
            if waiter_sched.pi.blocked_on != Some(registration) {
                task_runtime::fatal_invariant(0x5049_1203, waiter_core.id().as_u64() as usize);
            }
            waiter_sched.pi.blocked_on = None;
        }
        // SAFETY: the mutex wait lock detached this node and the task PI lock
        // cleared the only registration which could name it. Keeping
        // `blocked_on` installed through `publish_lock_top_change()` matches
        // Linux's wait_lock + pi_lock transaction: owner-top propagation may
        // still inspect the waiter until the tree update is globally complete.
        unsafe { waiter_core.pi_wait_nodes().return_lock_waiter(removed) };
        waiter_core
            .pi_wait_state()
            .clear_top(registration.generation);
        Ok(registration)
    }

    /// Requeues one blocked waiter after its effective urgency changed.
    ///
    /// The normal order is mutex wait lock then task PI lock. Chain propagation
    /// begins from a task PI lock, so it mirrors Linux step [5]: try the wait
    /// lock, drop/retry on contention, and revalidate `blocked_on` after success.
    fn refresh_blocked_waiter_key(
        &self,
        waiter_core: &Arc<ThreadCore>,
    ) -> Result<PiWaiterRefresh, TaskError> {
        loop {
            let donation = self.pi_donation(waiter_core)?;
            let mut waiter_sched = waiter_core.sched().lock();
            let Some(registration) = waiter_sched.pi.blocked_on else {
                return Ok(PiWaiterRefresh {
                    owner: None,
                    changed: false,
                });
            };
            let urgency = waiter_core.effective_pi_wait_urgency();
            let Some(mut lock_state) = (unsafe {
                // SAFETY: `blocked_on` pins the mutex identity until this
                // registration is removed under the same task PI lock.
                registration.lock.try_lock_state()
            }) else {
                drop(waiter_sched);
                core::hint::spin_loop();
                continue;
            };
            if waiter_sched.pi.blocked_on != Some(registration) {
                continue;
            }
            let old_donation = lock_state
                .waiters
                .donation(registration.key)
                .ok_or(TaskError::InvalidPiState)?;
            if urgency == registration.key.urgency && donation.same_source(&old_donation) {
                return Ok(PiWaiterRefresh {
                    owner: unsafe { registration.lock.core() }.owner_snapshot().owner(),
                    changed: false,
                });
            }
            let old_top = lock_state.waiters.first_entry();
            let node = lock_state
                .waiters
                .remove(registration.key)
                .ok_or(TaskError::InvalidPiState)?;
            let new_key = PiWaitKey::new(urgency, registration.key.sequence, waiter_core.id());
            lock_state.waiters.insert(new_key, donation, node);
            let owner = unsafe { registration.lock.core() }.owner_snapshot().owner();
            if let Err(error) = self.publish_lock_top_change(
                owner,
                old_top.clone(),
                lock_state.waiters.first_entry(),
                Some(PiWaiterRekey {
                    waiter: waiter_core.id(),
                    registration,
                    new_key,
                }),
            ) {
                let node = lock_state
                    .waiters
                    .remove(new_key)
                    .ok_or(TaskError::InvalidPiState)?;
                lock_state
                    .waiters
                    .insert(registration.key, old_donation, node);
                return Err(error);
            }
            let current = waiter_sched.pi.blocked_on.as_mut().unwrap_or_else(|| {
                task_runtime::fatal_invariant(0x5049_1210, waiter_core.id().as_u64() as usize)
            });
            if *current != registration {
                task_runtime::fatal_invariant(0x5049_1205, waiter_core.id().as_u64() as usize);
            }
            current.key = new_key;
            return Ok(PiWaiterRefresh {
                owner,
                changed: true,
            });
        }
    }

    /// Propagates one already committed PI owner update through blocked owners.
    ///
    /// No invocation owns more than one task PI lock plus one mutex wait lock.
    /// `origin_lock` enables Linux's full chain walk after a new edge is
    /// installed so a concurrent indirect cycle is detected and rolled back.
    pub(super) fn recompute_pi_chain(
        &self,
        start: ThreadId,
        origin_lock: Option<PiMutexRaw>,
        top_task: ThreadId,
    ) -> Result<(), TaskError> {
        self.recompute_pi_chain_bounded(start, origin_lock, top_task, self.config.pi_chain_limit())
    }

    /// Propagates a committed PI removal or priority change through the
    /// existing dependency graph.
    ///
    /// Linux uses the minimum chain-walk mode for these adjustments: the
    /// configured admission bound applies to adding a new dependency, not to
    /// restoring an already accepted graph after unlock, cancellation, or a
    /// policy change. The fixed thread capacity is the structural upper bound
    /// of an acyclic in-kernel wait graph.
    fn recompute_pi_cleanup_chain(
        &self,
        start: ThreadId,
        top_task: ThreadId,
    ) -> Result<(), TaskError> {
        self.recompute_pi_chain_bounded(start, None, top_task, self.config.thread_capacity())
    }

    fn recompute_pi_chain_bounded(
        &self,
        start: ThreadId,
        origin_lock: Option<PiMutexRaw>,
        top_task: ThreadId,
        limit: usize,
    ) -> Result<(), TaskError> {
        let mut current = start;
        for depth in 1..=limit {
            let current_core = self.pi_thread_core(current)?;
            let Some(_activity) = current_core.try_scheduler_activity() else {
                return Err(TaskError::InvalidPiWaitState(
                    PiWaitStateError::ExitedParticipant,
                ));
            };
            let blocked_on = current_core.sched().lock().pi.blocked_on;
            if origin_lock.is_some_and(|origin| {
                blocked_on.is_some_and(|registration| registration.lock == origin)
            }) {
                return Err(TaskError::PiCycle);
            }
            let refresh = self.refresh_blocked_waiter_key(&current_core)?;
            if !refresh.changed && origin_lock.is_none() {
                return Ok(());
            }
            let Some(owner) = refresh.owner else {
                return Ok(());
            };
            if owner == top_task {
                return Err(TaskError::PiCycle);
            }
            if depth == limit {
                return Err(TaskError::PiChainLimit { limit });
            }
            current = owner;
        }
        Err(TaskError::PiChainLimit { limit })
    }

    pub(super) fn propagate_pi_waiter_key_after_policy_change(
        &self,
        thread: ThreadId,
    ) -> Result<(), TaskError> {
        let core = self.pi_thread_core(thread)?;
        let refresh = self.refresh_blocked_waiter_key(&core)?;
        let Some(owner) = refresh.owner else {
            return Ok(());
        };
        if !refresh.changed {
            return Ok(());
        }
        if owner == thread {
            return Err(TaskError::PiCycle);
        }
        self.recompute_pi_cleanup_chain(owner, thread)
    }

    fn remove_registered_waiter(
        &self,
        waiter_core: &Arc<ThreadCore>,
        lock: PiMutexRaw,
        generation: u64,
        reject_handoff_top: bool,
    ) -> Result<Option<ThreadId>, TaskError> {
        let mut lock_state = unsafe {
            // SAFETY: the token or rollback caller retains the mutex identity.
            lock.lock_state()
        };
        let core = unsafe {
            // SAFETY: identical lifetime contract to the waiter-tree guard.
            lock.core()
        };
        let snapshot = core.owner_snapshot();
        if !snapshot.has_waiters() {
            return Err(TaskError::InvalidPiState);
        }
        let registration = waiter_core
            .sched()
            .lock()
            .pi
            .blocked_on
            .filter(|registration| {
                registration.lock == lock && registration.generation == generation
            })
            .ok_or(TaskError::InvalidPiState)?;
        if reject_handoff_top
            && snapshot.is_ownerless()
            && lock_state.waiters.first() == Some(registration.key)
        {
            return Err(TaskError::InvalidPiState);
        }
        if !lock_state.waiters.contains(registration.key) {
            return Err(TaskError::InvalidPiState);
        }
        let owner = snapshot.owner();
        self.remove_lock_waiter(&mut lock_state, owner, waiter_core, generation)?;
        if lock_state.waiters.is_empty() {
            if let Some(owner) = owner {
                core.clear_waiters_bit(owner).unwrap_or_else(|_| {
                    task_runtime::fatal_invariant(0x5049_1215, waiter_core.id().as_u64() as usize)
                });
            } else {
                core.publish_unlocked();
            }
        }
        Ok(owner)
    }

    /// Registers one contender in the mutex-owned PI waiter tree.
    pub fn pi_mutex_lock_slow<'lock>(
        &self,
        lock: PiMutexRef<'lock>,
        waiter: ThreadId,
        sequence: u64,
    ) -> Result<PiMutexLockResult<'lock>, TaskError> {
        let _preempt = PreemptScope::enter();
        let waiter_core = self.pi_thread_core(waiter)?;
        let Some(_waiter_activity) = waiter_core.try_scheduler_activity() else {
            return Err(TaskError::InvalidPiWaitState(
                PiWaitStateError::ExitedParticipant,
            ));
        };
        {
            let sched = waiter_core.sched().lock();
            if sched.lifecycle.state() == ThreadState::Exited {
                return Err(TaskError::InvalidPiWaitState(
                    PiWaitStateError::ExitedParticipant,
                ));
            }
            if sched.pi.blocked_on.is_some() {
                return Err(TaskError::InvalidPiWaitState(
                    PiWaitStateError::WaiterAlreadyBlocked,
                ));
            }
        }
        let lock_raw = lock.raw();
        let mutex_core = lock.core();
        let urgency = waiter_core.effective_pi_wait_urgency();
        let donation = self.pi_donation(&waiter_core)?;
        let key = PiWaitKey::new(urgency, sequence, waiter);
        let mut lock_state = lock.lock_state();
        loop {
            let snapshot = mutex_core.owner_snapshot();
            if snapshot.is_unlocked() {
                if !lock_state.waiters.is_empty() {
                    return Err(TaskError::InvalidPiWaitState(
                        PiWaitStateError::StaleSchedulerOwnership,
                    ));
                }
                if mutex_core.try_acquire_snapshot(snapshot, waiter)? {
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
            let initial_owner = owner
                .map(|owner| {
                    self.pi_thread_core(owner).map_err(|_| {
                        TaskError::InvalidPiWaitState(PiWaitStateError::ExitedParticipant)
                    })
                })
                .transpose()?;
            let _owner_activity = if let Some(owner) = initial_owner.as_ref() {
                Some(
                    owner
                        .try_scheduler_activity()
                        .ok_or(TaskError::InvalidPiWaitState(
                            PiWaitStateError::ExitedParticipant,
                        ))?,
                )
            } else {
                None
            };
            if !mutex_core.try_mark_waiters(snapshot) {
                continue;
            }
            let generation = match waiter_core.pi_wait_state().begin() {
                Ok(generation) => generation,
                Err(error) => {
                    if !snapshot.has_waiters() {
                        mutex_core
                            .clear_waiters_bit(owner.expect("an owned mutex must retain its owner"))
                            .unwrap_or_else(|_| {
                                task_runtime::fatal_invariant(
                                    0x5049_120c,
                                    waiter_core.id().as_u64() as usize,
                                )
                            });
                    }
                    return Err(error);
                }
            };
            if let Err(error) = self.insert_lock_waiter(
                &mut lock_state,
                owner,
                &waiter_core,
                PiWaitRegistration {
                    lock: lock_raw,
                    key,
                    generation,
                },
                donation,
            ) {
                if lock_state.waiters.is_empty() {
                    mutex_core
                        .clear_waiters_bit(owner.expect("an empty waiter tree must retain owner"))
                        .unwrap_or_else(|_| {
                            task_runtime::fatal_invariant(
                                0x5049_120d,
                                waiter_core.id().as_u64() as usize,
                            )
                        });
                }
                return Err(error);
            }
            drop(lock_state);
            if let Some(owner) = owner
                && let Err(error) = self.recompute_pi_chain(owner, Some(lock_raw), waiter)
            {
                let rollback_owner = self
                    .remove_registered_waiter(&waiter_core, lock_raw, generation, false)
                    .unwrap_or_else(|_| {
                        task_runtime::fatal_invariant(
                            0x5049_120e,
                            waiter_core.id().as_u64() as usize,
                        )
                    });
                if let Some(rollback_owner) = rollback_owner {
                    self.recompute_pi_cleanup_chain(rollback_owner, waiter)
                        .unwrap_or_else(|_| {
                            task_runtime::fatal_invariant(
                                0x5049_120f,
                                waiter_core.id().as_u64() as usize,
                            )
                        });
                }
                return Err(error);
            }
            drop(_owner_activity);
            drop(_waiter_activity);
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
        let _preempt = PreemptScope::enter();
        let owner =
            self.remove_registered_waiter(&token.core, token.lock, token.generation, true)?;
        if let Some(owner) = owner {
            self.recompute_pi_cleanup_chain(owner, token.thread_id())?;
        }
        Ok(())
    }

    /// Publishes an ownerless handoff and wakes the current top waiter.
    pub fn pi_mutex_release(
        &self,
        lock: PiMutexRef<'_>,
        old_owner: ThreadId,
    ) -> Result<(), TaskError> {
        let _preempt = PreemptScope::enter();
        let old_owner_core = self.pi_thread_core(old_owner)?;
        let selected = {
            let mutex_core = lock.core();
            let lock_state = lock.lock_state();
            let snapshot = mutex_core.owner_snapshot();
            if snapshot.owner() != Some(old_owner) || !snapshot.has_waiters() {
                return Err(TaskError::InvalidPiState);
            }
            let selected_entry = lock_state
                .waiters
                .first_entry()
                .ok_or(TaskError::InvalidPiState)?;
            let selected_key = selected_entry.0;
            let selected = selected_entry
                .1
                .waiter_core()
                .ok_or(TaskError::InvalidPiState)?;
            let registration = selected
                .sched()
                .lock()
                .pi
                .blocked_on
                .filter(|registration| registration.lock.id() == lock.id())
                .ok_or(TaskError::InvalidPiState)?;
            if !selected.pi_wait_state().can_grant(registration.generation)
                || !old_owner_core
                    .sched()
                    .lock()
                    .pi
                    .donors
                    .contains(selected_key)
            {
                return Err(TaskError::InvalidPiState);
            }
            self.replace_owner_lock_top(old_owner, Some(selected_entry), None)?;
            mutex_core.publish_ownerless();
            drop(lock_state);
            selected
        };
        self.recompute_pi_cleanup_chain(old_owner, selected.id())
            .unwrap_or_else(|_| {
                task_runtime::fatal_invariant(0x5049_1211, old_owner.as_u64() as usize)
            });
        let selected_id = selected.id();
        match self.wake_thread_direct(selected, None) {
            WakeResult::Notified | WakeResult::AlreadyPending => Ok(()),
            WakeResult::Exited | WakeResult::Unavailable => task_runtime::fatal_invariant(
                PI_RELEASE_WAKE_INVARIANT,
                selected_id.as_u64() as usize,
            ),
        }
    }

    /// Claims an ownerless handoff selected for this waiter.
    pub fn pi_mutex_claim(&self, token: &PiWaitToken<'_>) -> Result<(), TaskError> {
        let _preempt = PreemptScope::enter();
        let claimant = token.thread_id();
        let lock = token.lock;
        let mut lock_state = unsafe {
            // SAFETY: the borrowed token keeps the physical mutex core live.
            lock.lock_state()
        };
        let mutex_core = unsafe {
            // SAFETY: the token lifetime is borrowed from this mutex core.
            lock.core()
        };
        if !mutex_core.owner_snapshot().is_ownerless() {
            return Err(TaskError::InvalidPiState);
        }
        let registration = token
            .core
            .sched()
            .lock()
            .pi
            .blocked_on
            .filter(|registration| {
                registration.lock == lock && registration.generation == token.generation
            })
            .ok_or(TaskError::InvalidPiState)?;
        if lock_state.waiters.first() != Some(registration.key)
            || !token
                .core
                .pi_wait_state()
                .can_grant(registration.generation)
        {
            return Err(TaskError::InvalidPiState);
        }
        let registration =
            self.remove_lock_waiter(&mut lock_state, None, &token.core, token.generation)?;
        debug_assert_eq!(registration.generation, token.generation);
        if let Some(top) = lock_state.waiters.first_entry() {
            self.replace_owner_lock_top(claimant, None, Some(top))
                .unwrap_or_else(|_| {
                    task_runtime::fatal_invariant(0x5049_1212, claimant.as_u64() as usize)
                });
        }
        mutex_core.publish_owner(claimant, !lock_state.waiters.is_empty());
        token
            .core
            .pi_wait_state()
            .grant(token.generation)
            .unwrap_or_else(|_| {
                task_runtime::fatal_invariant(0x5049_1213, claimant.as_u64() as usize)
            });
        drop(lock_state);
        self.recompute_pi_cleanup_chain(claimant, claimant)
            .unwrap_or_else(|_| {
                task_runtime::fatal_invariant(0x5049_1214, claimant.as_u64() as usize)
            });
        Ok(())
    }
}

fn pi_inherited_policy(base: SchedulePolicy, donor: SchedulePolicy) -> Option<SchedulePolicy> {
    match donor {
        SchedulePolicy::Deadline(policy) => Some(SchedulePolicy::Deadline(policy)),
        SchedulePolicy::Fifo { priority } | SchedulePolicy::RoundRobin { priority, .. } => {
            Some(match base {
                SchedulePolicy::RoundRobin { quantum_ns, .. } => SchedulePolicy::RoundRobin {
                    priority,
                    quantum_ns,
                },
                SchedulePolicy::KernelStop | SchedulePolicy::Deadline(_) => return None,
                SchedulePolicy::Fair { .. } | SchedulePolicy::Fifo { .. } => {
                    SchedulePolicy::Fifo { priority }
                }
            })
        }
        SchedulePolicy::Fair { .. } => matches!(base, SchedulePolicy::Fair { .. }).then_some(donor),
        SchedulePolicy::KernelStop => None,
    }
}
