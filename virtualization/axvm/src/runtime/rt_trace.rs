// Copyright 2026 The Axvisor Team
//
// Licensed under the Apache License, Version 2.0.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::VmExit;

const LOG_INTERVAL: u64 = 1024;
const SLOW_VMEXIT_NS: u64 = 1_000_000;
const TRACKED_INTERRUPT_VECTORS: usize = 1024;

static IRQ_QUEUE_COUNT: AtomicU64 = AtomicU64::new(0);
static IRQ_DRAIN_COUNT: AtomicU64 = AtomicU64::new(0);
static IRQ_DISPATCH_TOTAL_NS: AtomicU64 = AtomicU64::new(0);
static IRQ_DISPATCH_MAX_NS: AtomicU64 = AtomicU64::new(0);
static HOST_IRQ_DISPATCH_COUNT: AtomicU64 = AtomicU64::new(0);
static HOST_IRQ_DISPATCH_TOTAL_NS: AtomicU64 = AtomicU64::new(0);
static HOST_IRQ_DISPATCH_MAX_NS: AtomicU64 = AtomicU64::new(0);
static HOST_IRQ_HANDLER_COUNT: AtomicU64 = AtomicU64::new(0);
static HOST_IRQ_HANDLER_TOTAL_NS: AtomicU64 = AtomicU64::new(0);
static HOST_IRQ_HANDLER_MAX_NS: AtomicU64 = AtomicU64::new(0);
static VCPU_WAKE_COUNT: AtomicU64 = AtomicU64::new(0);
static VCPU_RESUME_COUNT: AtomicU64 = AtomicU64::new(0);
static VCPU_RESUME_TOTAL_NS: AtomicU64 = AtomicU64::new(0);
static VCPU_RESUME_MAX_NS: AtomicU64 = AtomicU64::new(0);
static VMEXIT_COUNT: AtomicU64 = AtomicU64::new(0);
static SLOW_VMEXIT_COUNT: AtomicU64 = AtomicU64::new(0);
static MAX_VMEXIT_NS: AtomicU64 = AtomicU64::new(0);
static LAST_INTERRUPT_VECTOR: AtomicUsize = AtomicUsize::new(0);
static VMEXIT_VECTOR_COUNT: [AtomicU64; TRACKED_INTERRUPT_VECTORS] =
    [const { AtomicU64::new(0) }; TRACKED_INTERRUPT_VECTORS];
static VMEXIT_VECTOR_TOTAL_NS: [AtomicU64; TRACKED_INTERRUPT_VECTORS] =
    [const { AtomicU64::new(0) }; TRACKED_INTERRUPT_VECTORS];
static VMEXIT_VECTOR_MAX_NS: [AtomicU64; TRACKED_INTERRUPT_VECTORS] =
    [const { AtomicU64::new(0) }; TRACKED_INTERRUPT_VECTORS];
static TIMER_NO_DEADLINE_COUNT: AtomicU64 = AtomicU64::new(0);
static TIMER_NOT_DUE_COUNT: AtomicU64 = AtomicU64::new(0);
static TIMER_LOCKED_COUNT: AtomicU64 = AtomicU64::new(0);
static TIMER_EXPIRED_COUNT: AtomicU64 = AtomicU64::new(0);
static VCPU_LOOKUP_COUNT: AtomicU64 = AtomicU64::new(0);
static DEVICE_LOOKUP_COUNT: AtomicU64 = AtomicU64::new(0);
static VCPU_BIND_COUNT: AtomicU64 = AtomicU64::new(0);
static VCPU_UNBIND_COUNT: AtomicU64 = AtomicU64::new(0);
static PREPARED_VCPU_ENTRY_COUNT: AtomicU64 = AtomicU64::new(0);
static LEGACY_VCPU_ENTRY_COUNT: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "rt-trace")]
static VCPU_STATE_LOCK_COUNT: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug)]
pub(crate) enum TimerCheckPath {
    #[cfg(not(feature = "rt-shared-wait-baseline"))]
    NoDeadline,
    #[cfg(not(feature = "rt-shared-wait-baseline"))]
    NotDue,
    Locked {
        expired: bool,
    },
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum VcpuRunPathEvent {
    Lookup,
    DeviceLookup,
    Bind,
    Unbind,
    #[cfg(not(feature = "rt-shared-wait-baseline"))]
    PreparedEntry,
    LegacyEntry,
}

#[inline(always)]
pub(crate) const fn enabled() -> bool {
    cfg!(feature = "rt-trace")
}

#[inline(always)]
pub(crate) fn now_ns() -> u64 {
    if enabled() {
        crate::host::arceos::monotonic_time_nanos()
    } else {
        0
    }
}

pub(crate) fn trace_vcpu_task_affinity(
    vm_id: usize,
    vcpu_id: usize,
    requested_mask: Option<usize>,
    effective_mask: usize,
) {
    info!(
        "rt-trace: VM[{vm_id}] VCpu[{vcpu_id}] affinity requested={requested_mask:?} \
         effective={effective_mask:#x}"
    );
}

#[inline(always)]
pub(crate) fn trace_interrupt_queued(
    vm_id: usize,
    vcpu_id: usize,
    vector: usize,
    target_cpu: usize,
) {
    if !enabled() {
        return;
    }
    let count = IRQ_QUEUE_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    LAST_INTERRUPT_VECTOR.store(vector, Ordering::Relaxed);
    if count == 1 || count.is_multiple_of(LOG_INTERVAL) {
        info!(
            "rt-trace: interrupt queued count={count} VM[{vm_id}] VCpu[{vcpu_id}] vector={vector} \
             target_cpu={target_cpu}"
        );
    }
}

#[inline(always)]
pub(crate) fn trace_interrupt_drained(vm_id: usize, vcpu_id: usize, latencies_ns: &[u64]) {
    if !enabled() {
        return;
    }
    let drained = latencies_ns.len();
    if drained == 0 {
        return;
    }
    let previous_count = IRQ_DRAIN_COUNT.fetch_add(drained as u64, Ordering::Relaxed);
    let count = previous_count + drained as u64;
    let batch_total: u64 = latencies_ns.iter().copied().sum();
    let batch_max = latencies_ns.iter().copied().max().unwrap_or(0);
    IRQ_DISPATCH_TOTAL_NS.fetch_add(batch_total, Ordering::Relaxed);
    update_max(&IRQ_DISPATCH_MAX_NS, batch_max);
    if previous_count == 0 || crossed_log_interval(previous_count, count) {
        // Load total before count. Every total update is preceded by its count
        // update, so this order may understate the average under concurrency but
        // cannot combine a newer total with a stale sample count.
        let total = IRQ_DISPATCH_TOTAL_NS.load(Ordering::Relaxed);
        let snapshot_count = IRQ_DRAIN_COUNT.load(Ordering::Relaxed);
        let max_ns = IRQ_DISPATCH_MAX_NS.load(Ordering::Relaxed);
        info!(
            "rt-trace: interrupt dispatch total={snapshot_count} VM[{vm_id}] VCpu[{vcpu_id}] \
             batch={drained} avg_ns={} batch_max_ns={batch_max} max_ns={max_ns}",
            total / snapshot_count
        );
    }
}

#[inline(always)]
pub(crate) fn trace_vcpu_wake(vm_id: usize, vcpu_id: usize, woke_waiter: bool) {
    if !enabled() {
        return;
    }
    let count = VCPU_WAKE_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    if woke_waiter || count == 1 || count.is_multiple_of(LOG_INTERVAL) {
        info!(
            "rt-trace: vcpu wake count={count} VM[{vm_id}] VCpu[{vcpu_id}] \
             woke_waiter={woke_waiter}"
        );
    }
}

#[inline(always)]
pub(crate) fn trace_host_irq_dispatch(vector: usize, latency_ns: u64) {
    if !enabled() {
        return;
    }
    let count = HOST_IRQ_DISPATCH_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    HOST_IRQ_DISPATCH_TOTAL_NS.fetch_add(latency_ns, Ordering::Relaxed);
    update_max(&HOST_IRQ_DISPATCH_MAX_NS, latency_ns);
    if count == 1 || count.is_multiple_of(LOG_INTERVAL) {
        let total = HOST_IRQ_DISPATCH_TOTAL_NS.load(Ordering::Relaxed);
        let snapshot_count = HOST_IRQ_DISPATCH_COUNT.load(Ordering::Relaxed);
        let max_ns = HOST_IRQ_DISPATCH_MAX_NS.load(Ordering::Relaxed);
        info!(
            "rt-trace: host irq dispatch count={snapshot_count} vector={vector} \
             latency_ns={latency_ns} avg_ns={} max_ns={max_ns}",
            total / snapshot_count
        );
    }
}

#[inline(always)]
pub(crate) fn trace_host_irq_handler(vector: Option<usize>, latency_ns: u64) {
    if !enabled() {
        return;
    }
    let count = HOST_IRQ_HANDLER_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    HOST_IRQ_HANDLER_TOTAL_NS.fetch_add(latency_ns, Ordering::Relaxed);
    update_max(&HOST_IRQ_HANDLER_MAX_NS, latency_ns);
    if count == 1 || count.is_multiple_of(LOG_INTERVAL) {
        let total = HOST_IRQ_HANDLER_TOTAL_NS.load(Ordering::Relaxed);
        let snapshot_count = HOST_IRQ_HANDLER_COUNT.load(Ordering::Relaxed);
        let max_ns = HOST_IRQ_HANDLER_MAX_NS.load(Ordering::Relaxed);
        info!(
            "rt-trace: host irq handler count={snapshot_count} vector={vector:?} \
             latency_ns={latency_ns} avg_ns={} max_ns={max_ns}",
            total / snapshot_count
        );
    }
}

#[inline(always)]
pub(crate) fn trace_vcpu_resumed(vm_id: usize, vcpu_id: usize, latency_ns: u64) {
    if !enabled() {
        return;
    }
    let count = VCPU_RESUME_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    VCPU_RESUME_TOTAL_NS.fetch_add(latency_ns, Ordering::Relaxed);
    update_max(&VCPU_RESUME_MAX_NS, latency_ns);
    if count == 1 || count.is_multiple_of(LOG_INTERVAL) {
        let total = VCPU_RESUME_TOTAL_NS.load(Ordering::Relaxed);
        let snapshot_count = VCPU_RESUME_COUNT.load(Ordering::Relaxed);
        let max_ns = VCPU_RESUME_MAX_NS.load(Ordering::Relaxed);
        info!(
            "rt-trace: vcpu resume count={snapshot_count} VM[{vm_id}] VCpu[{vcpu_id}] \
             latency_ns={latency_ns} avg_ns={} max_ns={max_ns}",
            total / snapshot_count
        );
    }
}

#[inline(always)]
pub(crate) fn trace_vmexit(
    vm_id: usize,
    vcpu_id: usize,
    elapsed_ns: u64,
    exit_reason: Option<&VmExit>,
) {
    if !enabled() {
        return;
    }
    if let Some(VmExit::ExternalInterrupt { vector }) = exit_reason {
        trace_vmexit_vector(vm_id, vcpu_id, *vector as usize, elapsed_ns);
    }
    let count = VMEXIT_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    update_max(&MAX_VMEXIT_NS, elapsed_ns);
    let slow_count = if elapsed_ns >= SLOW_VMEXIT_NS {
        SLOW_VMEXIT_COUNT.fetch_add(1, Ordering::Relaxed) + 1
    } else {
        0
    };
    let sample_slow = slow_count != 0 && slow_count.is_multiple_of(LOG_INTERVAL);
    if count <= 32 || count.is_power_of_two() || sample_slow {
        let max_ns = MAX_VMEXIT_NS.load(Ordering::Relaxed);
        let last_vector = LAST_INTERRUPT_VECTOR.load(Ordering::Relaxed);
        info!(
            "rt-trace: vmexit count={count} slow_count={slow_count} VM[{vm_id}] VCpu[{vcpu_id}] \
             elapsed_ns={elapsed_ns} max_ns={max_ns} last_irq_vector={last_vector} \
             reason={exit_reason:?}"
        );
    }
}

#[inline(always)]
pub(crate) fn trace_vcpu_run_path(vm_id: usize, vcpu_id: usize, event: VcpuRunPathEvent) {
    if !enabled() {
        return;
    }
    let (event_name, count) = match event {
        VcpuRunPathEvent::Lookup => (
            "lookup",
            VCPU_LOOKUP_COUNT.fetch_add(1, Ordering::Relaxed) + 1,
        ),
        VcpuRunPathEvent::DeviceLookup => (
            "device-lookup",
            DEVICE_LOOKUP_COUNT.fetch_add(1, Ordering::Relaxed) + 1,
        ),
        VcpuRunPathEvent::Bind => ("bind", VCPU_BIND_COUNT.fetch_add(1, Ordering::Relaxed) + 1),
        VcpuRunPathEvent::Unbind => (
            "unbind",
            VCPU_UNBIND_COUNT.fetch_add(1, Ordering::Relaxed) + 1,
        ),
        #[cfg(not(feature = "rt-shared-wait-baseline"))]
        VcpuRunPathEvent::PreparedEntry => (
            "prepared-entry",
            PREPARED_VCPU_ENTRY_COUNT.fetch_add(1, Ordering::Relaxed) + 1,
        ),
        VcpuRunPathEvent::LegacyEntry => (
            "legacy-entry",
            LEGACY_VCPU_ENTRY_COUNT.fetch_add(1, Ordering::Relaxed) + 1,
        ),
    };
    if count == 1 || count.is_power_of_two() {
        info!(
            "rt-trace: vcpu run path event={event_name} count={count} VM[{vm_id}] VCpu[{vcpu_id}] \
             lookup={} device_lookup={} bind={} unbind={} prepared_entry={} legacy_entry={}",
            VCPU_LOOKUP_COUNT.load(Ordering::Relaxed),
            DEVICE_LOOKUP_COUNT.load(Ordering::Relaxed),
            VCPU_BIND_COUNT.load(Ordering::Relaxed),
            VCPU_UNBIND_COUNT.load(Ordering::Relaxed),
            PREPARED_VCPU_ENTRY_COUNT.load(Ordering::Relaxed),
            LEGACY_VCPU_ENTRY_COUNT.load(Ordering::Relaxed),
        );
    }
}

#[inline(always)]
#[cfg(feature = "rt-trace")]
pub(crate) fn trace_vcpu_state_locks(vm_id: usize, vcpu_id: usize, locks: u64) {
    if !enabled() {
        return;
    }
    let entries = PREPARED_VCPU_ENTRY_COUNT.load(Ordering::Relaxed)
        + LEGACY_VCPU_ENTRY_COUNT.load(Ordering::Relaxed);
    let total = VCPU_STATE_LOCK_COUNT.fetch_add(locks, Ordering::Relaxed) + locks;
    if entries <= 32 || entries.is_power_of_two() {
        info!(
            "rt-trace: vcpu state locks VM[{vm_id}] VCpu[{vcpu_id}] last={locks} total={total} \
             entries={entries}"
        );
    }
}

#[inline(always)]
fn trace_vmexit_vector(vm_id: usize, vcpu_id: usize, vector: usize, elapsed_ns: u64) {
    let Some(count) = VMEXIT_VECTOR_COUNT.get(vector) else {
        return;
    };
    let count = count.fetch_add(1, Ordering::Relaxed) + 1;
    let total =
        VMEXIT_VECTOR_TOTAL_NS[vector].fetch_add(elapsed_ns, Ordering::Relaxed) + elapsed_ns;
    update_max(&VMEXIT_VECTOR_MAX_NS[vector], elapsed_ns);
    if count == 1 || count.is_power_of_two() {
        let max_ns = VMEXIT_VECTOR_MAX_NS[vector].load(Ordering::Relaxed);
        info!(
            "rt-trace: vmexit vector={vector} count={count} VM[{vm_id}] VCpu[{vcpu_id}] \
             elapsed_ns={elapsed_ns} avg_ns={} max_ns={max_ns}",
            total / count
        );
    }
}

#[inline(always)]
pub(crate) fn trace_timer_check(path: TimerCheckPath) {
    if !enabled() {
        return;
    }
    let (path_name, count) = match path {
        #[cfg(not(feature = "rt-shared-wait-baseline"))]
        TimerCheckPath::NoDeadline => (
            "no-deadline",
            TIMER_NO_DEADLINE_COUNT.fetch_add(1, Ordering::Relaxed) + 1,
        ),
        #[cfg(not(feature = "rt-shared-wait-baseline"))]
        TimerCheckPath::NotDue => (
            "not-due",
            TIMER_NOT_DUE_COUNT.fetch_add(1, Ordering::Relaxed) + 1,
        ),
        TimerCheckPath::Locked { expired } => {
            if expired {
                TIMER_EXPIRED_COUNT.fetch_add(1, Ordering::Relaxed);
            }
            (
                if expired {
                    "locked-expired"
                } else {
                    "locked-empty"
                },
                TIMER_LOCKED_COUNT.fetch_add(1, Ordering::Relaxed) + 1,
            )
        }
    };
    if count == 1 || count.is_power_of_two() {
        info!(
            "rt-trace: timer check path={path_name} count={count} no_deadline={} not_due={} \
             locked={} expired={}",
            TIMER_NO_DEADLINE_COUNT.load(Ordering::Relaxed),
            TIMER_NOT_DUE_COUNT.load(Ordering::Relaxed),
            TIMER_LOCKED_COUNT.load(Ordering::Relaxed),
            TIMER_EXPIRED_COUNT.load(Ordering::Relaxed),
        );
    }
}

#[inline(always)]
fn update_max(max: &AtomicU64, value: u64) {
    let mut current = max.load(Ordering::Relaxed);
    while value > current {
        match max.compare_exchange_weak(current, value, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break,
            Err(next) => current = next,
        }
    }
}

#[inline(always)]
fn crossed_log_interval(previous: u64, current: u64) -> bool {
    previous / LOG_INTERVAL != current / LOG_INTERVAL
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_crossing_log_interval_is_sampled() {
        assert!(crossed_log_interval(1023, 1025));
        assert!(!crossed_log_interval(1025, 1030));
    }
}
