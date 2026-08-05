use super::*;

mod delivery;
mod idle_pull;
mod lifecycle;
mod load_summary;
mod owner;
mod run_queue;
mod scheduler;

pub(crate) use delivery::PreparedMigrationDelivery;
pub(crate) use idle_pull::IdlePullReservation;
pub use lifecycle::CpuLifecycleState;
pub(crate) use lifecycle::{CpuRemotePublication, OwnedCpuRemotePublication};
pub use owner::CpuLocalOwnerBorrow;
pub(crate) use run_queue::CpuRunQueueState;
#[cfg(feature = "qperf-metrics")]
pub(crate) use run_queue::WakePreemptionDecision;

/// Stable cross-CPU publication endpoint for one scheduler owner.
///
/// This object owns the IRQ-safe target runqueue, atomic delivery state, and
/// intrusive owner-control inboxes. Owner-only runtime accounting and switch
/// tail state remain in [`CpuLocal`].
#[derive(Debug)]
pub struct CpuRemote {
    owner: CpuId,
    run_queue: IrqTicketLock<CpuRunQueueState>,
    /// Linux `dl_rq.extra_bw`: root-domain bandwidth published for this rq.
    deadline_extra_bw_scaled: AtomicU64,
    owner_state: owner::OwnerState,
    publication: lifecycle::CpuPublicationState,
    scheduler: scheduler::SchedulerDoorbellState,
    load: load_summary::RemoteLoadState,
    idle_pull: idle_pull::IdlePullState,
    delivery: delivery::RemoteDeliveryState,
}

impl CpuRemote {
    pub(crate) fn create(owner: CpuId, config: TaskSystemConfig) -> Arc<Self> {
        let deadline_max_bw_scaled =
            u64::from(config.deadline_cap_percent()) * crate::DEADLINE_UTILIZATION_SCALE / 100;
        Arc::new(Self {
            owner,
            run_queue: IrqTicketLock::new(CpuRunQueueState::new(owner, config)),
            deadline_extra_bw_scaled: AtomicU64::new(deadline_max_bw_scaled),
            owner_state: owner::OwnerState::new(),
            publication: lifecycle::CpuPublicationState::new(),
            scheduler: scheduler::SchedulerDoorbellState::new(),
            load: load_summary::RemoteLoadState::new(),
            idle_pull: idle_pull::IdlePullState::new(),
            delivery: delivery::RemoteDeliveryState::new(),
        })
    }

    /// Acquires the target CPU runqueue with local IRQs disabled.
    ///
    /// Thread scheduler state must be acquired before this lock whenever one
    /// transaction needs both. Owner-only switch-tail state is never protected
    /// by this lock and must not escape its CPU-local scheduler baton.
    pub(crate) fn lock_run_queue(&self) -> IrqTicketGuard<'_, CpuRunQueueState> {
        self.run_queue.lock()
    }

    pub(crate) fn publish_deadline_extra_bw(&self, extra_bw_scaled: u64) {
        self.deadline_extra_bw_scaled
            .store(extra_bw_scaled, Ordering::Release);
    }

    pub(crate) fn deadline_extra_bw_scaled(&self) -> u64 {
        self.deadline_extra_bw_scaled.load(Ordering::Acquire)
    }
}

include!("remote/tests.rs");
