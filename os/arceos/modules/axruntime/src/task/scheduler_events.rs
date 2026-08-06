//! Physical scheduler timer and IPI event delivery.

use core::sync::atomic::{AtomicU64, Ordering};

#[cfg(any(feature = "ipi", feature = "wake-ipi", test))]
use ax_task::runtime::RuntimeStatus;

#[cfg(any(feature = "ipi", feature = "wake-ipi"))]
use super::with_current_cpu_pin;

const TASK_CLOCK_EVENT_IRQ_BUDGET: usize = 64;

static TASK_TIMER_IRQ_COUNT: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "qperf-metrics")]
static SCHEDULER_IPI_SEND_COUNT: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "qperf-metrics")]
static SCHEDULER_IPI_CONSUME_COUNT: AtomicU64 = AtomicU64::new(0);

/// Aggregate scheduler delivery counters for feature-gated qperf diagnostics.
#[cfg(feature = "qperf-metrics")]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct QperfRuntimeSchedulerMetricsSnapshot {
    pub task: ax_task::QperfSchedulerMetricsSnapshot,
    pub scheduler_ipi_sends: u64,
    pub scheduler_ipi_consumes: u64,
    pub clockevent_irqs: u64,
}

/// Result of publishing one logical scheduler IPI generation.
#[cfg(any(feature = "ipi", feature = "wake-ipi", test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SchedulerIpiPublication {
    Notify { generation: u64 },
    Coalesced { generation: u64 },
}

#[cfg(any(feature = "ipi", feature = "wake-ipi", test))]
impl SchedulerIpiPublication {
    const fn needs_notification(self) -> bool {
        matches!(self, Self::Notify { .. })
    }

    const fn generation(self) -> u64 {
        match self {
            Self::Notify { generation } | Self::Coalesced { generation } => generation,
        }
    }
}

/// One scheduler generation claimed at shared-IPI entry.
#[cfg(any(feature = "ipi", feature = "wake-ipi"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SchedulerIpiClaim {
    generation: u64,
}

#[cfg(any(feature = "ipi", feature = "wake-ipi"))]
impl SchedulerIpiClaim {
    pub(crate) const fn generation(self) -> u64 {
        self.generation
    }
}

/// Allocation-free generation transport for the shared physical IPI vector.
///
/// `delivered_generation` is copied from ax-task's authoritative scheduler
/// request state; this transport never creates or acknowledges logical work.
/// `edge_armed` owns the one physical notification covering the latest
/// delivered generation. Clearing the edge before reading the generation lets
/// a concurrent producer arm a fresh edge while the current handler drains
/// work, matching Linux irq_work's PENDING-before-callback rule.
#[cfg(any(feature = "ipi", feature = "wake-ipi"))]
pub(super) struct SchedulerIpiDoorbell {
    delivered_generation: AtomicU64,
    edge_armed: core::sync::atomic::AtomicBool,
}

#[cfg(any(feature = "ipi", feature = "wake-ipi"))]
impl SchedulerIpiDoorbell {
    pub(super) const fn new() -> Self {
        Self {
            delivered_generation: AtomicU64::new(0),
            edge_armed: core::sync::atomic::AtomicBool::new(false),
        }
    }

    pub(super) fn publish(&self, generation: u64) -> SchedulerIpiPublication {
        assert_ne!(
            generation, 0,
            "scheduler request generation must be nonzero"
        );
        let previous = self
            .delivered_generation
            .fetch_max(generation, Ordering::AcqRel);
        if generation <= previous {
            return SchedulerIpiPublication::Coalesced { generation };
        }
        if self
            .edge_armed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            SchedulerIpiPublication::Notify { generation }
        } else {
            SchedulerIpiPublication::Coalesced { generation }
        }
    }

    pub(super) fn claim(&self) -> Option<SchedulerIpiClaim> {
        if !self.edge_armed.swap(false, Ordering::AcqRel) {
            return None;
        }
        let generation = self.delivered_generation.load(Ordering::Acquire);
        Some(SchedulerIpiClaim { generation })
    }

    pub(super) fn is_pending(&self) -> bool {
        self.edge_armed.load(Ordering::Acquire)
    }

    pub(super) fn reset_for_offline(&self) {
        assert!(
            !self.edge_armed.load(Ordering::Acquire),
            "scheduler IPI edge must quiesce before CPU offline"
        );
        self.delivered_generation.store(0, Ordering::Release);
    }
}

#[cfg(any(feature = "ipi", feature = "wake-ipi"))]
#[ax_percpu::def_percpu]
static SCHEDULER_IPI_DOORBELL: SchedulerIpiDoorbell = SchedulerIpiDoorbell::new();

/// Returns the aggregate number of scheduler timer interrupts since boot.
pub fn timer_irq_count() -> u64 {
    TASK_TIMER_IRQ_COUNT.load(Ordering::Relaxed)
}

/// Returns aggregate task and physical-delivery counters without locking.
#[cfg(feature = "qperf-metrics")]
pub fn qperf_runtime_scheduler_metrics_snapshot() -> QperfRuntimeSchedulerMetricsSnapshot {
    QperfRuntimeSchedulerMetricsSnapshot {
        task: ax_task::qperf_scheduler_metrics_snapshot(),
        scheduler_ipi_sends: SCHEDULER_IPI_SEND_COUNT.load(Ordering::Relaxed),
        scheduler_ipi_consumes: SCHEDULER_IPI_CONSUME_COUNT.load(Ordering::Relaxed),
        clockevent_irqs: timer_irq_count(),
    }
}

#[cfg(all(feature = "qperf-metrics", any(feature = "ipi", feature = "wake-ipi")))]
pub(super) fn record_scheduler_ipi_send() {
    SCHEDULER_IPI_SEND_COUNT.fetch_add(1, Ordering::Relaxed);
}

/// Performs bounded task accounting and publishes a sticky reschedule request.
#[cfg(feature = "irq")]
pub(crate) fn on_clock_event(
    now: ax_task::runtime::MonotonicInstant,
    scheduler_tick: bool,
) -> ax_task::TaskClockEventOutcome {
    TASK_TIMER_IRQ_COUNT.fetch_add(1, Ordering::Relaxed);
    account_clock_event(now, scheduler_tick)
}

#[cfg(feature = "irq")]
fn account_clock_event(
    now: ax_task::runtime::MonotonicInstant,
    scheduler_tick: bool,
) -> ax_task::TaskClockEventOutcome {
    match ax_task::on_clock_event_with_scheduler_tick(
        now,
        TASK_CLOCK_EVENT_IRQ_BUDGET,
        scheduler_tick,
    ) {
        Ok(outcome) => outcome,
        Err(error) => panic!("task clockevent accounting failed: {error}"),
    }
}

/// Consumes scheduler delivery ownership from the shared physical IPI vector.
#[cfg(any(feature = "ipi", feature = "wake-ipi"))]
pub(crate) fn claim_scheduler_ipi_doorbell() -> Option<SchedulerIpiClaim> {
    // SAFETY: the IPI handler pins the current CPU for the complete operation.
    let consumed = unsafe {
        with_current_cpu_pin(|pin| {
            SCHEDULER_IPI_DOORBELL.with_current(pin, SchedulerIpiDoorbell::claim)
        })
    };
    #[cfg(feature = "qperf-metrics")]
    if consumed.is_some() {
        SCHEDULER_IPI_CONSUME_COUNT.fetch_add(1, Ordering::Relaxed);
    }
    consumed
}

#[cfg(any(feature = "ipi", feature = "wake-ipi"))]
pub(crate) fn current_scheduler_ipi_doorbell_pending() -> bool {
    // SAFETY: CPU-offline preparation owns the IRQ-excluded current CPU.
    unsafe {
        with_current_cpu_pin(|pin| {
            SCHEDULER_IPI_DOORBELL.with_current(pin, SchedulerIpiDoorbell::is_pending)
        })
    }
}

#[cfg(any(feature = "ipi", feature = "wake-ipi"))]
pub(crate) fn reset_current_scheduler_ipi_doorbell_for_offline() {
    // SAFETY: CPU-offline preparation owns the IRQ-excluded current CPU after
    // remote scheduler admission and every physical edge have quiesced.
    unsafe {
        with_current_cpu_pin(|pin| {
            SCHEDULER_IPI_DOORBELL.with_current(pin, SchedulerIpiDoorbell::reset_for_offline)
        })
    }
}

#[cfg(any(feature = "ipi", feature = "wake-ipi"))]
pub(super) fn publish_scheduler_ipi_doorbell(
    cpu_id: usize,
    generation: u64,
) -> Result<SchedulerIpiPublication, RuntimeStatus> {
    if generation == 0 {
        return Err(RuntimeStatus::InvalidArgument);
    }
    let Ok(cpu_index) = ax_percpu::CpuIndex::try_from(cpu_id) else {
        return Err(RuntimeStatus::InvalidArgument);
    };
    let Ok(area) = ax_percpu::area(cpu_index) else {
        return Err(RuntimeStatus::NotInitialized);
    };
    // SAFETY: runtime per-CPU areas are permanent after publication, and the
    // doorbell is an atomic object explicitly designed for remote publication.
    Ok(unsafe { SCHEDULER_IPI_DOORBELL.remote_ptr(area).as_ref() }.publish(generation))
}

#[cfg(any(feature = "ipi", feature = "wake-ipi", test))]
pub(super) fn publish_then_notify_scheduler_ipi(
    publish: impl FnOnce() -> Result<SchedulerIpiPublication, RuntimeStatus>,
    notify: impl FnOnce(),
) -> RuntimeStatus {
    let publication = match publish() {
        Ok(publication) => publication,
        Err(status) => return status,
    };
    debug_assert_ne!(publication.generation(), 0);
    if publication.needs_notification() {
        notify();
    }
    RuntimeStatus::Success
}
