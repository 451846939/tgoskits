use super::*;
use crate::{
    SchedulerClockEvent, scheduler_clock_event, scheduler_time_advance, scheduler_time_reached,
};

mod deadline_state;
mod dispatch_state;
mod drain_state;

use deadline_state::SchedulerDeadlinePublicationState;

/// Scheduler state that is created explicitly and mutated only by its owner CPU.
///
/// The object is `!Unpin`; runtimes store it in per-CPU pinned allocations and
/// publish it only after registration has completed.
#[derive(Debug)]
pub struct CpuLocal {
    owner: CpuId,
    remote: Arc<CpuRemote>,
    rt_bandwidth: Arc<RootRtBandwidth>,
    dispatch: dispatch_state::OwnerDispatchState,
    task_deadlines: deadline_state::LocalTaskDeadlineState,
    drain: drain_state::OwnerDrainScratch,
    _pinned: PhantomPinned,
}

impl CpuLocal {
    pub(crate) fn create(
        owner: CpuId,
        config: TaskSystemConfig,
        remote: Arc<CpuRemote>,
        rt_bandwidth: Arc<RootRtBandwidth>,
    ) -> Pin<Box<Self>> {
        debug_assert_eq!(owner, remote.owner());
        Box::pin(Self {
            owner,
            remote,
            rt_bandwidth,
            dispatch: dispatch_state::OwnerDispatchState::new(config),
            task_deadlines: deadline_state::LocalTaskDeadlineState::new(config),
            drain: drain_state::OwnerDrainScratch::new(config),
            _pinned: PhantomPinned,
        })
    }

    /// Returns the logical processor that exclusively owns the run queue.
    pub const fn owner(&self) -> CpuId {
        self.owner
    }

    /// Returns whether registration and online publication have completed.
    pub fn is_online(&self) -> bool {
        self.remote.is_online()
    }

    pub(crate) fn remote(&self) -> &Arc<CpuRemote> {
        &self.remote
    }

    /// Returns the currently executing non-idle thread, if any.
    pub const fn current(&self) -> Option<ThreadId> {
        self.dispatch.current
    }

    pub(crate) fn current_core(&self) -> Option<&Arc<ThreadCore>> {
        self.dispatch.current_core.as_ref()
    }

    /// Clones a strong handle for the currently executing thread.
    ///
    /// This owner-side lookup never consults the generation registry. The
    /// stable core retained by `CpuLocal` pins the registry record and any OS
    /// extension until the returned handle is dropped.
    pub fn current_thread_handle(&self) -> Result<ThreadHandle, TaskError> {
        self.dispatch
            .current_core
            .as_ref()
            .map(|core| ThreadHandle::from_core(Arc::clone(core)))
            .ok_or(TaskError::NoRunnableThread)
    }

    /// Returns the configured CPU idle thread, if any.
    pub const fn idle(&self) -> Option<ThreadId> {
        self.dispatch.idle
    }

    /// Returns the number of runnable non-idle threads.
    pub(crate) fn runnable_count(&self) -> usize {
        self.remote.lock_run_queue().len()
    }

    pub(crate) fn is_quiescent_for_offline(&self) -> bool {
        let run_queue = self.remote.lock_run_queue();
        (self.dispatch.current.is_none() || self.dispatch.current == self.dispatch.idle)
            && run_queue.len() == 0
            && run_queue.deadline_members_are_empty()
            && self.task_deadlines.queue.is_empty()
            && self.task_deadlines.expired_count == 0
            && self.dispatch.switch_handoff.is_none()
            && self.remote.is_quiescent_for_offline()
    }

    /// Publishes a sticky reschedule request from task or IRQ context.
    pub fn request_reschedule(&self) {
        self.remote.request_reschedule();
    }

    pub(crate) fn request_scheduler_work(&self) {
        self.remote.request_scheduler_work();
    }

    pub(crate) fn defer_scheduler_work(&self) {
        self.remote.defer_scheduler_work();
    }

    /// Tests the sticky reschedule request without clearing it.
    pub fn needs_reschedule(&self) -> bool {
        self.remote.needs_reschedule()
    }

    /// Returns the scheduler thread capacity prepared for this CPU.
    pub fn thread_capacity(&self) -> usize {
        self.task_deadlines.queue.capacity()
    }

    /// Returns the bounded scheduler safe-point work budget.
    pub const fn batch_limit(&self) -> usize {
        self.drain.batch_limit()
    }

    pub(crate) fn clear_current(self: Pin<&mut Self>) {
        // SAFETY: the scheduler owns this pinned object; projecting disjoint
        // fields does not move the CpuLocal identity.
        let this = unsafe { self.get_unchecked_mut() };
        this.remote.lock_run_queue().set_current(None);
        this.dispatch.current = None;
        this.dispatch.current_core = None;
        this.dispatch.current_dispatch = None;
        this.remote.publish_current_thread(None);
    }

    pub(crate) fn set_current_core(self: Pin<&mut Self>, core: Arc<ThreadCore>) {
        let id = core.id();
        // SAFETY: the scheduler owns this pinned object; projecting disjoint
        // fields does not move the CpuLocal identity.
        let this = unsafe { self.get_unchecked_mut() };
        this.dispatch.current = Some(id);
        this.dispatch.current_core = Some(core);
        this.remote.publish_current_thread(Some(id));
        this.remote.mark_scheduler_ready();
    }

    pub(crate) fn install_dispatch(self: Pin<&mut Self>, dispatch: CurrentDispatch) {
        // SAFETY: replacing owner state cannot move CpuLocal. The remote
        // scheduling snapshot is committed under the runqueue lock before a
        // concurrent wake may compare preemption priority.
        let this = unsafe { self.get_unchecked_mut() };
        let role = if this.dispatch.idle == Some(dispatch.thread) {
            DispatchRole::DedicatedIdle
        } else {
            DispatchRole::Task
        };
        let snapshot = dispatch.schedule_snapshot(role);
        let mut run_queue = this.remote.lock_run_queue();
        this.dispatch.current_dispatch = Some(dispatch);
        run_queue.set_current(Some(snapshot));
    }

    pub(crate) fn take_dispatch(self: Pin<&mut Self>) -> Option<CurrentDispatch> {
        // SAFETY: taking owner state cannot move CpuLocal.
        let this = unsafe { self.get_unchecked_mut() };
        let mut run_queue = this.remote.lock_run_queue();
        let dispatch = this.dispatch.current_dispatch.take();
        run_queue.set_current(None);
        dispatch
    }

    /// Reads the lock-free lifecycle published by the current dispatch.
    pub(crate) fn current_lifecycle_state(&self) -> Option<ThreadState> {
        self.dispatch
            .current_dispatch
            .as_ref()
            .map(|dispatch| dispatch.runtime_core().state())
    }

    pub(crate) fn charge_current_dispatch(
        self: Pin<&mut Self>,
        runtime_ns: u64,
        reclaimed_ns: u64,
    ) -> Result<DispatchCharge, TaskError> {
        // SAFETY: the owner scheduler serializes this pinned runqueue state.
        // These disjoint projections avoid reference-count traffic on every
        // runtime-accounting update.
        let this = unsafe { self.get_unchecked_mut() };
        let remote = &this.remote;
        let dispatch = &mut this.dispatch;
        let mut run_queue = remote.lock_run_queue();
        let now_ns = run_queue.update_clock().task_nanos();
        Self::charge_current_dispatch_locked(
            remote,
            dispatch,
            &mut run_queue,
            now_ns,
            runtime_ns,
            reclaimed_ns,
        )
    }

    fn charge_current_dispatch_locked(
        remote: &CpuRemote,
        dispatch_state: &mut dispatch_state::OwnerDispatchState,
        run_queue: &mut CpuRunQueueState,
        now_ns: u64,
        runtime_ns: u64,
        reclaimed_ns: u64,
    ) -> Result<DispatchCharge, TaskError> {
        let dedicated_idle =
            dispatch_state.current.is_some() && dispatch_state.current == dispatch_state.idle;
        let dispatch = dispatch_state
            .current_dispatch
            .as_mut()
            .ok_or(TaskError::NoRunnableThread)?;
        if dedicated_idle {
            dispatch.account_dedicated_idle_until(now_ns);
            run_queue.set_current(Some(
                dispatch.schedule_snapshot(DispatchRole::DedicatedIdle),
            ));
            return Ok(DispatchCharge::default());
        }
        let bandwidth = run_queue.deadline_bandwidth();
        let inactive_bw_scaled = bandwidth.inactive_bw_scaled();
        let max_bw_scaled = bandwidth.max_bw_scaled();
        let extra_bw_scaled = remote.deadline_extra_bw_scaled();
        let grub_reclaimed_ns = dispatch.grub_reclaimed_ns(
            runtime_ns,
            inactive_bw_scaled,
            extra_bw_scaled,
            max_bw_scaled,
        );
        remote.charge_busy_runtime(runtime_ns);
        let charge = dispatch.charge(
            runtime_ns,
            now_ns,
            reclaimed_ns.saturating_add(grub_reclaimed_ns),
        );
        let current_policy = dispatch.policy;
        let current_fair = dispatch.entity.fair();
        let rt_quota_exempt = dispatch.rt_quota_exempt;
        run_queue.update_fair_virtual_time(current_fair);
        run_queue.set_current(Some(dispatch.schedule_snapshot(DispatchRole::Task)));
        let rt_quota_exhausted = if matches!(
            current_policy,
            SchedulePolicy::Fifo { .. } | SchedulePolicy::RoundRobin { .. }
        ) {
            run_queue.charge_rt_runtime(runtime_ns)
        } else {
            false
        };
        if charge.slice_expired
            || charge.deadline_overrun
            || (rt_quota_exhausted && !rt_quota_exempt)
        {
            remote.request_reschedule();
        }
        Ok(charge)
    }

    pub(crate) fn settle_current_dispatch(
        self: Pin<&mut Self>,
        reclaimed_ns: u64,
    ) -> Result<(DispatchCharge, RunQueueClockSnapshot), TaskError> {
        // SAFETY: the owner scheduler serializes this pinned runqueue state.
        let this = unsafe { self.get_unchecked_mut() };
        let remote = &this.remote;
        let dispatch = &mut this.dispatch;
        let mut run_queue = remote.lock_run_queue();
        let clock = run_queue
            .clock_snapshot()
            .ok_or(TaskError::InvalidConfiguration)?;
        Self::settle_current_dispatch_locked(remote, dispatch, &mut run_queue, clock, reclaimed_ns)
    }

    pub(crate) fn settle_current_dispatch_with_clock(
        self: Pin<&mut Self>,
        reclaimed_ns: u64,
    ) -> Result<(DispatchCharge, RunQueueClockSnapshot), TaskError> {
        // SAFETY: the owner scheduler serializes this pinned runqueue state.
        let this = unsafe { self.get_unchecked_mut() };
        let remote = &this.remote;
        let dispatch = &mut this.dispatch;
        let mut run_queue = remote.lock_run_queue();
        let clock = run_queue.update_clock();
        Self::settle_current_dispatch_locked(remote, dispatch, &mut run_queue, clock, reclaimed_ns)
    }

    fn settle_current_dispatch_locked(
        remote: &CpuRemote,
        dispatch: &mut dispatch_state::OwnerDispatchState,
        run_queue: &mut CpuRunQueueState,
        clock: RunQueueClockSnapshot,
        reclaimed_ns: u64,
    ) -> Result<(DispatchCharge, RunQueueClockSnapshot), TaskError> {
        let now_ns = clock.task_nanos();
        let runtime_ns = dispatch
            .current_dispatch
            .as_ref()
            .ok_or(TaskError::NoRunnableThread)?
            .unaccounted_runtime(now_ns);
        let charge = Self::charge_current_dispatch_locked(
            remote,
            dispatch,
            run_queue,
            now_ns,
            runtime_ns,
            reclaimed_ns,
        )?;
        Ok((charge, clock))
    }

    pub(crate) fn set_idle(self: Pin<&mut Self>, idle: ThreadId, core: Arc<ThreadCore>) {
        debug_assert_eq!(idle, core.id());
        // SAFETY: changing fields does not move this pinned object.
        let fields = unsafe { self.get_unchecked_mut() };
        fields.dispatch.idle = Some(idle);
        fields.dispatch.idle_core = Some(core);
        fields.remote.publish_idle_thread(idle);
        fields.remote.mark_scheduler_ready();
    }

    pub(crate) fn stage_switch_handoff(
        self: Pin<&mut Self>,
        previous: Arc<ThreadCore>,
        migration: Option<PreparedMigrationDelivery>,
    ) -> Result<(), TaskError> {
        let handoff = &mut self.dispatch_state_mut().switch_handoff;
        if handoff.is_some() {
            return Err(TaskError::InvalidConfiguration);
        }
        *handoff = Some(SwitchHandoff {
            previous,
            migration,
            runtime_tail_finished: false,
        });
        Ok(())
    }

    pub(crate) fn finish_switch_runtime_tail(
        self: Pin<&mut Self>,
        previous: ThreadId,
        migration_target: Option<CpuId>,
    ) -> Result<(), TaskError> {
        let handoff = self
            .dispatch_state_mut()
            .switch_handoff
            .as_mut()
            .ok_or(TaskError::InvalidConfiguration)?;
        if handoff.previous.id() != previous
            || handoff.migration_target() != migration_target
            || handoff.runtime_tail_finished
        {
            return Err(TaskError::InvalidConfiguration);
        }
        handoff.runtime_tail_finished = true;
        Ok(())
    }

    pub(crate) fn take_switch_handoff(self: Pin<&mut Self>) -> Option<SwitchHandoff> {
        self.dispatch_state_mut().switch_handoff.take()
    }

    pub(crate) fn switch_handoff(&self) -> Option<&SwitchHandoff> {
        self.dispatch.switch_handoff.as_ref()
    }

    pub(crate) fn claim_scheduler_request(self: Pin<&mut Self>) -> SchedulerRequestClaim {
        self.remote.claim_scheduler_request()
    }

    pub(crate) fn acknowledge_scheduler_request(&self, claim: SchedulerRequestClaim) {
        self.remote.acknowledge_scheduler_request(claim);
    }

    pub(crate) fn defer_park_preemption(&self, requested: bool) {
        self.remote.defer_park_preemption(requested);
    }

    pub(crate) fn finish_park_preemption(&self, resume_running: bool) {
        self.remote.finish_park_preemption(resume_running);
    }

    pub(crate) const fn dispatch_state(&self) -> &dispatch_state::OwnerDispatchState {
        &self.dispatch
    }

    pub(crate) fn dispatch_state_mut(
        self: Pin<&mut Self>,
    ) -> &mut dispatch_state::OwnerDispatchState {
        // SAFETY: the owner borrow is pinned, and OwnerDispatchState contains
        // no self-referential pointer that can move CpuLocal.
        &mut unsafe { self.get_unchecked_mut() }.dispatch
    }

    pub(crate) fn lock_run_queue(&self) -> IrqTicketGuard<'_, CpuRunQueueState> {
        self.remote.lock_run_queue()
    }

    pub(crate) fn update_rq_clock(&self) -> RunQueueClockSnapshot {
        self.remote.lock_run_queue().update_clock()
    }

    fn task_deadline_state_mut(
        self: Pin<&mut Self>,
    ) -> &mut deadline_state::LocalTaskDeadlineState {
        // SAFETY: the owner borrow is pinned, and moving neither the queue nor
        // its preallocated output storage can move CpuLocal.
        &mut unsafe { self.get_unchecked_mut() }.task_deadlines
    }

    pub(crate) fn drain_state_mut(self: Pin<&mut Self>) -> &mut drain_state::OwnerDrainScratch {
        // SAFETY: scratch buffers are owner-only and do not move CpuLocal.
        &mut unsafe { self.get_unchecked_mut() }.drain
    }

    pub(crate) const fn drain_state(&self) -> &drain_state::OwnerDrainScratch {
        &self.drain
    }

    #[cfg(test)]
    pub(crate) fn deadline_members_are_empty_for_test(&self) -> bool {
        self.remote.lock_run_queue().deadline_members_are_empty()
    }

    pub(crate) fn balance_request_node(&self) -> Pin<&'static InboxNode> {
        self.remote.balance_request_node()
    }

    #[cfg(test)]
    pub(crate) fn add_deadline_bandwidth(
        self: Pin<&mut Self>,
        utilization_scaled: u64,
        active: bool,
    ) {
        self.remote
            .lock_run_queue()
            .add_deadline_bandwidth(utilization_scaled, active);
    }

    /// Returns the owner runqueue's GRUB bandwidth accounting.
    pub fn deadline_bandwidth(&self) -> DeadlineBandwidthSnapshot {
        self.remote.lock_run_queue().deadline_bandwidth()
    }

    /// Returns Linux `dl_rq.extra_bw` published for this owner runqueue.
    pub fn deadline_extra_bw_scaled(&self) -> u64 {
        self.remote.deadline_extra_bw_scaled()
    }

    pub(crate) fn scheduler_work_due(
        self: Pin<&mut Self>,
        clock: RunQueueClockSnapshot,
        monotonic_now: MonotonicInstant,
    ) -> bool {
        let now_ns = clock.task_nanos();
        // SAFETY: the scheduler owns this pinned runqueue while refreshing RT
        // bandwidth periods and querying its next local event.
        let this = unsafe { self.get_unchecked_mut() };
        let scheduler_due = this
            .scheduler_deadline_ns(now_ns)
            .is_some_and(|deadline| scheduler_time_reached(now_ns, deadline));
        if scheduler_due {
            this.remote.request_reschedule();
        }
        scheduler_due || this.publish_fair_balance_due(monotonic_now)
    }

    pub(crate) fn next_oneshot_deadline(
        self: Pin<&mut Self>,
        clock: RunQueueClockSnapshot,
        monotonic_now: MonotonicInstant,
    ) -> Option<MonotonicDeadline> {
        let scheduler_now_ns = clock.task_nanos();
        // SAFETY: clockevent selection is an owner-only transition. The
        // mutable queue/scheduler projections cannot move CpuLocal.
        let this = unsafe { self.get_unchecked_mut() };
        let deferred_timer_backlog = this.remote.soft_timer_work_pending()
            && this
                .task_deadlines
                .queue
                .has_immediately_actionable_entry(monotonic_now);
        let timer = if deferred_timer_backlog {
            // A bounded hard-IRQ pass transferred ownership of the overdue
            // heap head to sticky soft-timer work. Re-arming that same head
            // would create an interrupt storm; the claimed scheduler work is
            // now the sole progress mechanism until the queue is drained.
            None
        } else {
            this.task_deadlines.queue.next_deadline()
        };
        let scheduler = match this
            .scheduler_deadline_ns(scheduler_now_ns)
            .map(|deadline| scheduler_clock_event(scheduler_now_ns, monotonic_now, deadline))
        {
            Some(SchedulerClockEvent::Due) => {
                // Linux does not start a scheduler hrtimer whose expiry has
                // already passed: the owning runqueue handles that state
                // immediately. The owner state remains the only deadline
                // authority; sticky work forces a scheduler safe point without
                // manufacturing a resolution-rate interrupt loop.
                this.remote.request_scheduler_work();
                None
            }
            Some(SchedulerClockEvent::Future(deadline)) => Some(deadline),
            None => None,
        };
        let fair_balance = this.fair_balance_clockevent_deadline(monotonic_now);
        let rt_period = this.rt_bandwidth.deadline_for(this.owner);
        [timer, scheduler, fair_balance, rt_period]
            .into_iter()
            .flatten()
            .min()
    }

    pub(crate) fn next_scheduler_deadline_update(
        mut self: Pin<&mut Self>,
        clock: RunQueueClockSnapshot,
        monotonic_now: MonotonicInstant,
    ) -> Result<SchedulerDeadlineUpdate, TaskError> {
        let publication = self
            .as_mut()
            .scheduler_deadline_publication(clock, monotonic_now);
        let task_deadlines = self.task_deadline_state_mut();
        if task_deadlines.publication == Some(publication) {
            return SchedulerDeadlineUpdate::try_new(
                task_deadlines.generation,
                publication.deadline,
            )
            .ok_or(TaskError::InvalidConfiguration);
        }
        Self::commit_scheduler_deadline_publication(task_deadlines, publication)
    }

    pub(crate) fn next_scheduler_deadline_update_if_changed(
        mut self: Pin<&mut Self>,
        clock: RunQueueClockSnapshot,
        monotonic_now: MonotonicInstant,
    ) -> Result<Option<SchedulerDeadlineUpdate>, TaskError> {
        let publication = self
            .as_mut()
            .scheduler_deadline_publication(clock, monotonic_now);
        let task_deadlines = self.task_deadline_state_mut();
        if task_deadlines.publication == Some(publication) {
            return Ok(None);
        }
        Self::commit_scheduler_deadline_publication(task_deadlines, publication).map(Some)
    }

    fn scheduler_deadline_publication(
        mut self: Pin<&mut Self>,
        clock: RunQueueClockSnapshot,
        monotonic_now: MonotonicInstant,
    ) -> SchedulerDeadlinePublicationState {
        let deadline = self.as_mut().next_oneshot_deadline(clock, monotonic_now);
        SchedulerDeadlinePublicationState { deadline }
    }

    fn commit_scheduler_deadline_publication(
        task_deadlines: &mut deadline_state::LocalTaskDeadlineState,
        publication: SchedulerDeadlinePublicationState,
    ) -> Result<SchedulerDeadlineUpdate, TaskError> {
        task_deadlines.generation = task_deadlines
            .generation
            .checked_add(1)
            .ok_or(TaskError::InvalidConfiguration)?;
        let update =
            SchedulerDeadlineUpdate::try_new(task_deadlines.generation, publication.deadline)
                .ok_or(TaskError::InvalidConfiguration)?;
        task_deadlines.publication = Some(publication);
        Ok(update)
    }

    pub(crate) fn invalidate_scheduler_deadline_publication(self: Pin<&mut Self>) {
        self.task_deadline_state_mut().publication = None;
    }

    pub(crate) fn soft_timer_work_pending(&self) -> bool {
        self.remote.soft_timer_work_pending()
    }

    pub(crate) fn run_queue_clock(&self) -> Option<RunQueueClockSnapshot> {
        self.remote.lock_run_queue().clock_snapshot()
    }

    pub(crate) fn has_due_task_deadline(&self, now: MonotonicInstant) -> bool {
        self.task_deadlines
            .queue
            .has_immediately_actionable_entry(now)
    }

    #[cfg(test)]
    pub(crate) fn set_scheduler_deadline_generation_for_test(
        self: Pin<&mut Self>,
        generation: u64,
    ) {
        self.task_deadline_state_mut().generation = generation;
    }

    fn scheduler_deadline_ns(&mut self, now_ns: u64) -> Option<u64> {
        let mut next_deadline_ns = None;
        let run_queue = self.remote.lock_run_queue();
        if let Some(deadline) = run_queue.earliest_deadline_event_ns() {
            next_deadline_ns = earliest(next_deadline_ns, deadline);
        }

        let current_is_idle =
            self.dispatch.current.is_some() && self.dispatch.current == self.dispatch.idle;
        if !current_is_idle && let Some(dispatch) = self.dispatch.current_dispatch.as_ref() {
            let fair_slice_required = dispatch.entity.fair().is_none_or(|fair| {
                if fair.mode() == FairMode::Idle {
                    run_queue.has_idle_fair()
                } else {
                    run_queue.has_fair()
                }
            });
            if fair_slice_required && let Some(deadline) = dispatch.next_scheduler_event_ns(now_ns)
            {
                next_deadline_ns = earliest(next_deadline_ns, deadline);
            }
            if dispatch.is_rt()
                && !dispatch.rt_quota_exempt
                && let Some(remaining) = run_queue.rt_runtime_until_throttle()
            {
                next_deadline_ns =
                    earliest(next_deadline_ns, scheduler_time_advance(now_ns, remaining));
            }
        }
        next_deadline_ns
    }

    fn fair_balance_clockevent_deadline(
        &self,
        monotonic_now: MonotonicInstant,
    ) -> Option<MonotonicDeadline> {
        if !self.has_periodic_fair_balance_work() {
            return None;
        }
        if self.remote.publish_fair_balance_due(monotonic_now) {
            return None;
        }
        self.remote.fair_balance_deadline()
    }

    fn has_periodic_fair_balance_work(&self) -> bool {
        let run_queue = self.remote.lock_run_queue();
        let current_non_idle =
            self.dispatch.current.is_some() && self.dispatch.current != self.dispatch.idle;
        run_queue.has_fair()
            && run_queue
                .len()
                .saturating_add(usize::from(current_non_idle))
                > 1
    }

    /// Attempts to return a coherent remotely observable scheduling snapshot.
    pub fn try_load_summary(&self) -> Option<CpuLoadSummary> {
        self.remote.try_load_summary()
    }

    /// Attempts to return the remotely observable queued runnable count.
    pub fn try_runnable_summary(&self) -> Option<usize> {
        self.remote.try_runnable_summary()
    }

    pub(crate) fn publish_fair_balance_due(&self, now: MonotonicInstant) -> bool {
        self.has_periodic_fair_balance_work() && self.remote.publish_fair_balance_due(now)
    }

    pub(crate) fn fair_balance_pending(&self) -> bool {
        self.remote.fair_balance_pending()
    }

    pub(crate) fn reset_fair_balance(
        self: Pin<&mut Self>,
        now: MonotonicInstant,
        minimum_interval_ns: u64,
    ) {
        // SAFETY: this owner-only runqueue update does not move CpuLocal.
        let this = unsafe { self.get_unchecked_mut() };
        let interval_ns = minimum_interval_ns.max(1);
        this.dispatch.fair_balance_interval_ns = interval_ns;
        this.remote.defer_fair_balance(now, interval_ns);
    }

    pub(crate) fn backoff_fair_balance(
        self: Pin<&mut Self>,
        now: MonotonicInstant,
        minimum_interval_ns: u64,
        maximum_interval_ns: u64,
    ) {
        // SAFETY: this owner-only runqueue update does not move CpuLocal.
        let this = unsafe { self.get_unchecked_mut() };
        let minimum_interval_ns = minimum_interval_ns.max(1);
        let maximum_interval_ns = maximum_interval_ns.max(minimum_interval_ns);
        let current_interval_ns = this
            .dispatch
            .fair_balance_interval_ns
            .clamp(minimum_interval_ns, maximum_interval_ns);
        let next_interval_ns = current_interval_ns
            .saturating_mul(2)
            .min(maximum_interval_ns);
        this.dispatch.fair_balance_interval_ns = next_interval_ns;
        this.remote.defer_fair_balance(now, next_interval_ns);
    }

    /// Returns scheduler-internal owner access to the preallocated deadline heap.
    pub(crate) fn task_deadlines(self: Pin<&mut Self>) -> &mut TaskDeadlineQueue {
        // SAFETY: the pinned mutable owner borrow excludes every concurrent
        // timer consumer and does not move CpuLocal or its heap.
        &mut unsafe { self.get_unchecked_mut() }.task_deadlines.queue
    }

    /// Expires one bounded hard-IRQ timer batch and publishes soft-timer work.
    pub fn on_task_clock_event(
        self: Pin<&mut Self>,
        now: MonotonicInstant,
        budget: usize,
    ) -> TaskDeadlineExpireBatch {
        let mut this = self;
        let batch = this.as_mut().promote_due_task_deadlines(now, budget);
        if batch.pending() || batch.expired() != 0 {
            this.remote.publish_soft_timer_work();
        }
        batch
    }

    /// Moves one bounded batch into task-context storage without publishing a
    /// second scheduler request. The soft-timer worker owns publication for
    /// the complete begin/drain/finish transaction.
    pub(crate) fn promote_due_task_deadlines(
        self: Pin<&mut Self>,
        now: MonotonicInstant,
        budget: usize,
    ) -> TaskDeadlineExpireBatch {
        // SAFETY: hard-IRQ expiry owns this pinned CPU-local state. These
        // projections are disjoint and no projection is moved.
        let this = unsafe { self.get_unchecked_mut() };
        let batch_limit = this.drain.batch_limit();
        let task_deadlines = &mut this.task_deadlines;
        #[cfg(test)]
        {
            task_deadlines.expire_passes += 1;
        }
        let available = task_deadlines
            .expired_buffer
            .len()
            .saturating_sub(task_deadlines.expired_count);
        let request = TaskDeadlineExpireRequest::new(now, budget.min(batch_limit).min(available));
        let output = &mut task_deadlines.expired_buffer[task_deadlines.expired_count..];
        let batch = task_deadlines.queue.expire(request, output);
        task_deadlines.expired_count += batch.expired();
        batch
    }

    #[cfg(test)]
    pub(crate) const fn deadline_expire_passes_for_test(&self) -> usize {
        self.task_deadlines.expire_passes
    }

    pub(crate) fn begin_soft_timer_work(self: Pin<&mut Self>) -> bool {
        self.remote.begin_soft_timer_work()
    }

    pub(crate) fn finish_soft_timer_work(self: Pin<&mut Self>, pending: bool) {
        self.remote.finish_soft_timer_work(pending);
    }

    /// Copies expired timer events to task-context storage.
    ///
    /// Events that do not fit in `output` remain buffered for the next
    /// task-context drain.
    pub fn take_expired_task_deadlines(
        self: Pin<&mut Self>,
        output: &mut [ExpiredTaskDeadline],
    ) -> usize {
        let task_deadlines = self.task_deadline_state_mut();
        let buffered = task_deadlines.expired_count;
        let count = buffered.min(output.len());
        output[..count].copy_from_slice(&task_deadlines.expired_buffer[..count]);
        let remaining = buffered - count;
        task_deadlines
            .expired_buffer
            .copy_within(count..buffered, 0);
        task_deadlines.expired_buffer[remaining..buffered].fill(ExpiredTaskDeadline::EMPTY);
        task_deadlines.expired_count = remaining;
        count
    }

    pub(crate) fn take_one_expired_task_deadline(
        mut self: Pin<&mut Self>,
    ) -> Option<ExpiredTaskDeadline> {
        let mut event = [ExpiredTaskDeadline::EMPTY; 1];
        (self.as_mut().take_expired_task_deadlines(&mut event) == 1).then_some(event[0])
    }

    pub(crate) const fn has_expired_task_deadlines(&self) -> bool {
        self.task_deadlines.expired_count != 0
    }

    pub(crate) fn owns_buffered_expiration(&self, registration: &TaskDeadlineRegistration) -> bool {
        self.task_deadlines.expired_buffer[..self.task_deadlines.expired_count]
            .iter()
            .copied()
            .any(|event| {
                event.thread() == Some(registration.thread())
                    && event.token() == registration.token()
                    && event.deadline() == Some(registration.deadline())
                    && event.kind() == Some(registration.kind())
            })
    }

    /// Returns the owner-control publication endpoint for remote CPUs.
    pub fn owner_control_inbox(&self) -> &SchedulerInbox {
        self.remote.owner_control_inbox()
    }

    /// Reports pending remote work before idle or scheduler exit.
    pub fn has_remote_work(&self) -> bool {
        self.remote.has_remote_work()
    }

    /// Publishes the idle/polling state and performs the final WFI recheck.
    pub fn prepare_idle_wait(&self) -> bool {
        self.remote.prepare_idle_wait()
    }

    /// Clears idle/polling publication after WFI returns.
    pub fn finish_idle_wait(&self) {
        self.remote.finish_idle_wait();
    }

    /// Returns whether this CPU is between idle publication and WFI completion.
    pub fn is_idle_polling(&self) -> bool {
        self.remote.is_idle_polling()
    }
}

pub(super) fn earliest(current: Option<u64>, candidate: u64) -> Option<u64> {
    crate::earliest_scheduler_time(current, Some(candidate))
}
