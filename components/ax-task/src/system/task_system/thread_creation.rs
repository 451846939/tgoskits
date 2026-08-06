//! Transactional thread creation and initial CPU binding.

use super::*;

impl TaskSystem {
    /// Creates a thread in the [`ThreadState::New`] state.
    ///
    /// Deadline threads are admitted immediately and therefore must cover the
    /// complete online root domain.
    pub fn create_thread(&self, spec: ThreadSpec) -> Result<ThreadHandle, TaskError> {
        let policy = spec.policy();
        let affinity = spec
            .affinity()
            .cloned()
            .unwrap_or_else(|| CpuSet::all(self.config.cpu_count()));
        let unpublished = UnpublishedThreadGuard::new(self, spec);
        policy.validate()?;
        validate_affinity(&affinity, self.config.cpu_count())?;
        let (slot, generation, reservation) = {
            let mut state = self.state.lock();
            let mut root_domain = self.root_domain.lock();
            let reservation = root_domain.reserve_deadline(policy, &affinity)?;
            let (slot, generation) = match state.allocate_thread_slot(self.config.thread_capacity())
            {
                Ok(identity) => identity,
                Err(error) => {
                    root_domain.release_deadline(reservation);
                    return Err(error);
                }
            };
            state.slots[slot as usize].pending_deadline_reservation = reservation;
            (slot, generation, reservation)
        };
        let id = ThreadId::from_parts(slot, generation);

        // Linux embeds class nodes in task_struct before publication. Prepare
        // the Rust class-node indexes at the same cold construction boundary,
        // so a first wake or cross-CPU migration cannot allocate under rq
        // irqsave locks.
        for remote in &self.cpu_remotes {
            remote.lock_run_queue().prepare_thread_slot(slot as usize);
        }

        // Runtime construction may allocate, fault, or call into platform
        // code. Keep it outside the IRQ-disabled registry domain. The removed
        // slot is a private reservation until the short commit below.
        let entity = SchedulingEntity::new(policy, self.config.fair_slice_ns(), 0);
        let (extension, resources) = unpublished.into_owned_parts();
        let switch_extension = extension.as_ref().map(ThreadExtension::as_view);
        let scheduler_tick_work = extension
            .as_ref()
            .and_then(ThreadExtension::scheduler_tick_work);
        let sched = Arc::new(ThreadSchedCell::new(
            id,
            ThreadSchedState::new(
                policy,
                entity,
                affinity.clone(),
                reservation,
                resources.context(),
                resources.address_space(),
            ),
        ));
        let core = Arc::new(ThreadCore::new(
            id,
            policy,
            Arc::clone(&sched),
            switch_extension,
            scheduler_tick_work,
            Some(Arc::clone(&self.task_work)),
        ));
        let record = ThreadRecord {
            core: Arc::clone(&core),
            sched,
            resources,
            extension,
            blocked_on: None,
            pi_donors: PiWaitTree::new(),
            callbacks: ThreadCallbackState::new(),
        };
        let context = record.resources.context();
        if !context.is_none() {
            let status = task_runtime::bind_context_thread(ContextThreadBinding {
                context,
                publication: CurrentThreadPublication::from_core(id, &core),
            });
            if status != RuntimeStatus::Success {
                {
                    let mut state = self.state.lock();
                    let mut root_domain = self.root_domain.lock();
                    let failed_slot = &mut state.slots[slot as usize];
                    debug_assert_eq!(failed_slot.generation, generation);
                    debug_assert!(failed_slot.record.is_none());
                    debug_assert_eq!(failed_slot.pending_deadline_reservation, reservation);
                    failed_slot.pending_deadline_reservation = 0;
                    if advance_thread_slot_generation(failed_slot) {
                        state.free_slots.push(slot);
                    }
                    root_domain.release_deadline(reservation);
                }
                drop(core);
                self.release_thread_record(record);
                return Err(TaskError::RuntimeFailure(status as u32));
            }
        }

        let mut record = Some(record);
        let commit_error = {
            let mut state = self.state.lock();
            let mut root_domain = self.root_domain.lock();
            let is_deadline = matches!(policy, SchedulePolicy::Deadline(_));
            let topology_rejects_deadline = is_deadline && !affinity.covers(&root_domain.online);
            let admission_overcommitted = is_deadline && root_domain.admission_overcommitted();
            if topology_rejects_deadline || admission_overcommitted {
                let failed_slot = &mut state.slots[slot as usize];
                debug_assert_eq!(failed_slot.generation, generation);
                debug_assert!(failed_slot.record.is_none());
                debug_assert_eq!(failed_slot.pending_deadline_reservation, reservation);
                failed_slot.pending_deadline_reservation = 0;
                if advance_thread_slot_generation(failed_slot) {
                    state.free_slots.push(slot);
                }
                root_domain.release_deadline(reservation);
                Some(if topology_rejects_deadline {
                    TaskError::DeadlineAffinity
                } else {
                    TaskError::DeadlineAdmission
                })
            } else {
                let reserved_slot = &mut state.slots[slot as usize];
                debug_assert_eq!(reserved_slot.generation, generation);
                debug_assert!(reserved_slot.record.is_none());
                debug_assert_eq!(reserved_slot.pending_deadline_reservation, reservation);
                reserved_slot.pending_deadline_reservation = 0;
                reserved_slot.record = record.take();
                None
            }
        };
        if let Some(error) = commit_error {
            drop(core);
            self.release_thread_record(
                record.expect("rejected thread commit must retain its resource record"),
            );
            return Err(error);
        }
        Ok(ThreadHandle::from_core(core))
    }

    /// Transitions a new or waking thread to `Ready`.
    pub fn make_ready(&self, thread: ThreadId) -> Result<(), TaskError> {
        let state = self.state.lock();
        let record = state.thread_record(thread)?;
        let mut sched = record.sched.lock();
        sched.transition(&record.core, ThreadState::Ready)
    }

    /// Installs the CPU's already-running bootstrap execution context.
    ///
    /// This operation is used before a CPU is published online and performs no
    /// context switch. The runtime must call it exactly once with an empty
    /// `CpuLocal` current slot.
    pub fn install_bootstrap_thread(
        &self,
        mut cpu: Pin<&mut CpuLocal>,
        spec: ThreadSpec,
    ) -> Result<ThreadHandle, TaskError> {
        let unpublished = UnpublishedThreadGuard::new(self, spec);
        self.ensure_owner_cpu_context(&cpu)?;
        {
            let state = self.state.lock();
            let registration = state.cpu_registration(cpu.owner())?;
            if !Arc::ptr_eq(&registration.remote, cpu.remote()) {
                return Err(TaskError::InvalidRuntimeHandle);
            }
            if cpu.current().is_some() {
                return Err(TaskError::InvalidConfiguration);
            }
        }

        let thread = self.create_thread(unpublished.into_spec())?;
        let setup = (|| {
            let now_ns = cpu.update_rq_clock().task_nanos();
            let state = self.state.lock();
            let record = state.thread_record(thread.id())?;
            let core = Arc::clone(&record.core);
            let dispatch = {
                let mut sched = record.sched.lock();
                sched.transition(&core, ThreadState::Ready)?;
                sched.transition(&core, ThreadState::Running)?;
                let dispatch = Self::owner_dispatch(&core, &sched, now_ns)?;
                sched.placement.activate(cpu.owner());
                sched.placement.set_next_task(cpu.owner());
                core.set_wake_cpu_hint(cpu.owner());
                dispatch
            };
            cpu.as_mut().set_current_core(Arc::clone(&core));
            cpu.as_mut().install_dispatch(dispatch);
            drop(state);
            self.publish_owner_cpu_load_summary(cpu.as_mut());
            Ok(())
        })();
        if let Err(error) = setup {
            return match self.discard_unpublished_thread(thread) {
                Ok(()) => Err(error),
                Err(cleanup_error) => Err(cleanup_error),
            };
        }
        Ok(thread)
    }

    /// Creates and registers a dedicated CPU idle thread before online publish.
    pub fn register_idle_thread(
        &self,
        mut cpu: Pin<&mut CpuLocal>,
        spec: ThreadSpec,
    ) -> Result<ThreadHandle, TaskError> {
        let unpublished = UnpublishedThreadGuard::new(self, spec);
        self.ensure_owner_cpu_context(&cpu)?;
        if !matches!(
            unpublished.spec().policy(),
            SchedulePolicy::Fair {
                mode: crate::FairMode::Idle,
                ..
            }
        ) {
            return Err(TaskError::InvalidConfiguration);
        }
        {
            let state = self.state.lock();
            let registration = state.cpu_registration(cpu.owner())?;
            if !Arc::ptr_eq(&registration.remote, cpu.remote()) {
                return Err(TaskError::InvalidRuntimeHandle);
            }
            if cpu.idle().is_some() {
                return Err(TaskError::InvalidConfiguration);
            }
        }

        let thread = self.create_thread(unpublished.into_spec())?;
        let setup = self.make_ready(thread.id()).and_then(|()| {
            let state = self.state.lock();
            let core = Arc::clone(&state.thread_record(thread.id())?.core);
            cpu.as_mut().set_idle(thread.id(), core);
            Ok(())
        });
        if let Err(error) = setup {
            return match self.discard_unpublished_thread(thread) {
                Ok(()) => Err(error),
                Err(cleanup_error) => Err(cleanup_error),
            };
        }
        Ok(thread)
    }

    fn discard_unpublished_thread(&self, handle: ThreadHandle) -> Result<(), TaskError> {
        let record = {
            let mut state = self.state.lock();
            let mut root_domain = self.root_domain.lock();
            let (record, released) = state.remove_unpublished_thread_with_handle(&handle)?;
            root_domain.release_deadline(released);
            record
        };
        drop(handle);
        self.release_thread_record(record);
        Ok(())
    }
}
