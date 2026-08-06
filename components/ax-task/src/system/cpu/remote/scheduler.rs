use super::*;

const REQUEST_PREEMPT: u64 = 1 << 0;
const REQUEST_OWNER_WORK: u64 = 1 << 1;
const REQUEST_SOFT_TIMER: u64 = 1 << 2;
const REQUEST_REASON_MASK: u64 = REQUEST_PREEMPT | REQUEST_OWNER_WORK | REQUEST_SOFT_TIMER;
const REQUEST_ENTRY_MASK: u64 = REQUEST_PREEMPT | REQUEST_OWNER_WORK;
const REQUEST_IDLE_POLLING: u64 = 1 << 3;
const REQUEST_GENERATION_SHIFT: u32 = 8;
const REQUEST_FLAGS_MASK: u64 = (1 << REQUEST_GENERATION_SHIFT) - 1;
const REQUEST_GENERATION_MAX: u64 = u64::MAX >> REQUEST_GENERATION_SHIFT;
const DEFERRED_SCHEDULER_WORK_OFFLINE_INVARIANT: u32 = 0x4453_574f;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SchedulerRequestDelivery {
    /// The owner is in the IRQ-disabled idle polling region and will observe
    /// the sticky work bit before committing to sleep.
    PollingOwner,
    /// The runtime must publish a physical-delivery generation.
    ///
    /// The logical request generation is already committed in ax-task; the
    /// runtime transports that exact generation without inventing another.
    DoorbellRequired,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SchedulerRequestPublication {
    generation: u64,
    delivery: SchedulerRequestDelivery,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct SchedulerRequestClaim {
    generation: u64,
    preempt: bool,
}

impl SchedulerRequestClaim {
    pub(crate) const fn preempt_requested(self) -> bool {
        self.preempt
    }

    pub(crate) const fn merge(self, other: Self) -> Self {
        Self {
            generation: if self.generation > other.generation {
                self.generation
            } else {
                other.generation
            },
            preempt: self.preempt || other.preempt,
        }
    }
}

#[derive(Debug)]
pub(super) struct SchedulerDoorbellState {
    ready: AtomicBool,
    request: AtomicU64,
    acknowledged_generation: AtomicU64,
    park_preempt_deferred: AtomicBool,
}

impl SchedulerDoorbellState {
    pub(super) const fn new() -> Self {
        Self {
            ready: AtomicBool::new(false),
            request: AtomicU64::new(0),
            acknowledged_generation: AtomicU64::new(0),
            park_preempt_deferred: AtomicBool::new(false),
        }
    }
}

const fn request_generation(word: u64) -> u64 {
    word >> REQUEST_GENERATION_SHIFT
}

impl CpuRemote {
    pub(crate) fn mark_scheduler_ready(&self) {
        self.scheduler.ready.store(true, Ordering::Release);
    }

    pub(crate) fn is_scheduler_ready(&self) -> bool {
        self.scheduler.ready.load(Ordering::Acquire)
    }

    /// Publishes a sticky owner-CPU reschedule request.
    pub(crate) fn request_reschedule(&self) {
        let Some(_publication) = self.begin_publication() else {
            return;
        };
        self.request_reschedule_owned();
    }

    fn request_reschedule_owned(&self) -> SchedulerRequestPublication {
        self.publish_scheduler_request_owned(REQUEST_PREEMPT)
    }

    /// Publishes a remote preemption and rings the target doorbell only after
    /// the runqueue transaction has become visible.
    pub(crate) fn request_remote_reschedule(&self) {
        let Some(_publication) = self.begin_owner_delivery() else {
            return;
        };
        let _irq = IrqScope::enter();
        let publication = self.request_reschedule_owned();
        self.deliver_scheduler_work_owned(publication);
    }

    pub(crate) fn request_scheduler_work(&self) {
        let Some(_publication) = self.begin_owner_delivery() else {
            return;
        };
        self.request_scheduler_work_owned();
    }

    pub(super) fn request_scheduler_work_owned(&self) -> SchedulerRequestPublication {
        self.publish_scheduler_request_owned(REQUEST_OWNER_WORK)
    }

    fn publish_scheduler_request_owned(&self, reason: u64) -> SchedulerRequestPublication {
        debug_assert_ne!(reason & REQUEST_REASON_MASK, 0);
        let previous = self
            .scheduler
            .request
            .try_update(Ordering::AcqRel, Ordering::Acquire, |word| {
                let generation = request_generation(word).checked_add(1)?;
                if generation > REQUEST_GENERATION_MAX {
                    return None;
                }
                Some(
                    (generation << REQUEST_GENERATION_SHIFT) | (word & REQUEST_FLAGS_MASK) | reason,
                )
            })
            .unwrap_or_else(|_| panic!("scheduler request generation exhausted"));
        let generation = request_generation(previous) + 1;
        let delivery = if previous & REQUEST_IDLE_POLLING != 0 {
            SchedulerRequestDelivery::PollingOwner
        } else {
            SchedulerRequestDelivery::DoorbellRequired
        };
        SchedulerRequestPublication {
            generation,
            delivery,
        }
    }

    pub(in crate::system::cpu) fn publish_soft_timer_work(&self) {
        let _ = self.publish_scheduler_request_owned(REQUEST_SOFT_TIMER);
    }

    pub(crate) fn soft_timer_work_pending(&self) -> bool {
        self.scheduler.request.load(Ordering::Acquire) & REQUEST_SOFT_TIMER != 0
    }

    pub(in crate::system::cpu) fn begin_soft_timer_work(&self) -> bool {
        self.scheduler
            .request
            .fetch_and(!REQUEST_SOFT_TIMER, Ordering::AcqRel)
            & REQUEST_SOFT_TIMER
            != 0
    }

    pub(in crate::system::cpu) fn finish_soft_timer_work(&self, pending: bool) {
        // Only the owner CPU publishes deadline work, and both timer IRQ and
        // scheduler safe-point paths hold local IRQ exclusion while mutating
        // CpuLocal. The completed pass therefore owns the full publication
        // interval and may replace the sticky bit with its actual remainder.
        if pending {
            let _ = self.publish_scheduler_request_owned(REQUEST_SOFT_TIMER);
        }
    }

    pub(crate) fn kick_scheduler_work(&self) -> bool {
        let Some(_publication) = self.begin_owner_delivery() else {
            return false;
        };
        let _irq = IrqScope::enter();
        self.kick_scheduler_work_owned()
    }

    pub(super) fn kick_scheduler_work_owned(&self) -> bool {
        let publication = self.request_scheduler_work_owned();
        self.deliver_scheduler_work_owned(publication)
    }

    /// Rearms the physical doorbell after an owner-side bounded drain.
    ///
    /// Unlike producer delivery, this must not suppress a local notification:
    /// the current scheduler safe point has already consumed its delivery
    /// edge and is about to return. A remaining batch therefore needs a fresh
    /// interrupt even when the owner itself is the current CPU.
    pub(crate) fn defer_scheduler_work(&self) {
        let Some(_publication) = self.begin_owner_delivery() else {
            task_runtime::fatal_invariant(
                DEFERRED_SCHEDULER_WORK_OFFLINE_INVARIANT,
                self.owner.as_u32() as usize,
            );
        };
        let _irq = IrqScope::enter();
        let publication = self.request_scheduler_work_owned();
        self.ring_scheduler_doorbell(publication.generation);
    }

    pub(super) fn deliver_scheduler_work_owned(
        &self,
        publication: SchedulerRequestPublication,
    ) -> bool {
        if publication.delivery == SchedulerRequestDelivery::PollingOwner
            || self.current_cpu_will_service_local_work()
        {
            return true;
        }
        self.ring_scheduler_doorbell(publication.generation)
    }

    fn ring_scheduler_doorbell(&self, generation: u64) -> bool {
        match task_runtime::send_scheduler_ipi(RuntimeCpuId::new(self.owner.as_u32()), generation) {
            RuntimeStatus::Success => true,
            status => task_runtime::fatal_invariant(
                0x4950_4900 | status as u32,
                self.owner.as_u32() as usize,
            ),
        }
    }

    fn current_cpu_will_service_local_work(&self) -> bool {
        // Every caller retains an IrqScope from before this observation through
        // publication completion, so the runtime CPU identity cannot migrate.
        let current = unsafe { task_runtime::current_cpu_id() };
        if current.as_u32() != self.owner.as_u32() {
            return false;
        }
        // Publish into the architecture preemption word before suppressing a
        // self-IPI. Hard IRQ return consumes that state through its outer
        // preemption guard. Ordinary task publication instead converts the
        // final IRQ guard directly into the scheduler baton.
        task_runtime::publish_local_scheduler_work()
    }

    /// Tests the sticky reschedule request without consuming it.
    pub fn needs_reschedule(&self) -> bool {
        let request = self.scheduler.request.load(Ordering::Acquire);
        request & REQUEST_REASON_MASK != 0
            || request_generation(request)
                != self
                    .scheduler
                    .acknowledged_generation
                    .load(Ordering::Acquire)
    }

    pub(crate) fn claim_scheduler_request(&self) -> SchedulerRequestClaim {
        let request = self
            .scheduler
            .request
            .fetch_and(!REQUEST_ENTRY_MASK, Ordering::AcqRel);
        SchedulerRequestClaim {
            generation: request_generation(request),
            preempt: request & REQUEST_PREEMPT != 0,
        }
    }

    pub(crate) fn acknowledge_scheduler_request(&self, claim: SchedulerRequestClaim) {
        self.scheduler
            .acknowledged_generation
            .store(claim.generation, Ordering::Release);
        let request = self.scheduler.request.load(Ordering::Acquire);
        if self.has_remote_work() && request & REQUEST_ENTRY_MASK == 0 {
            self.request_scheduler_work();
        }
    }

    #[cfg(test)]
    pub(crate) fn take_preempt_requested(&self) -> bool {
        let claim = self.claim_scheduler_request();
        self.acknowledge_scheduler_request(claim);
        claim.preempt_requested()
    }

    #[cfg(test)]
    pub(crate) fn scheduler_request_state_for_test(&self) -> (u64, u64, u64) {
        let request = self.scheduler.request.load(Ordering::Acquire);
        (
            request_generation(request),
            self.scheduler
                .acknowledged_generation
                .load(Ordering::Acquire),
            request & REQUEST_REASON_MASK,
        )
    }

    pub(crate) fn defer_park_preemption(&self, requested: bool) {
        if requested {
            self.scheduler
                .park_preempt_deferred
                .store(true, Ordering::Release);
        }
    }

    pub(crate) fn finish_park_preemption(&self, resume_running: bool) {
        let deferred = self
            .scheduler
            .park_preempt_deferred
            .swap(false, Ordering::AcqRel);
        if resume_running && deferred {
            let _ = self.request_reschedule_owned();
        }
    }

    pub(crate) fn prepare_idle_wait(&self) -> bool {
        let previous = self
            .scheduler
            .request
            .fetch_or(REQUEST_IDLE_POLLING, Ordering::AcqRel);
        let may_wait = previous & REQUEST_REASON_MASK == 0
            && !self.needs_reschedule()
            && !self.has_remote_work()
            && self.try_runnable_summary() == Some(0);
        if !may_wait {
            self.finish_idle_wait();
        }
        may_wait
    }

    pub(crate) fn finish_idle_wait(&self) {
        self.scheduler
            .request
            .fetch_and(!REQUEST_IDLE_POLLING, Ordering::Release);
    }

    pub(crate) fn is_idle_polling(&self) -> bool {
        self.scheduler.request.load(Ordering::Acquire) & REQUEST_IDLE_POLLING != 0
    }

    pub(super) fn reset_scheduler_for_offline(&self) {
        self.scheduler.request.store(0, Ordering::Relaxed);
        self.scheduler
            .acknowledged_generation
            .store(0, Ordering::Relaxed);
        self.scheduler
            .park_preempt_deferred
            .store(false, Ordering::Relaxed);
    }
}
