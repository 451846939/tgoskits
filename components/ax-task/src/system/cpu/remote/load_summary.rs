use super::*;

const INCOMING_MIGRATION_OVERFLOW_INVARIANT: u32 = 0x4d49_474f;
const INCOMING_MIGRATION_RELEASE_INVARIANT: u32 = 0x4d49_4752;

struct RunQueueLoadPublication {
    current_key: Option<SchedulingKey>,
    pushable_key: Option<SchedulingKey>,
    runnable_count: usize,
    workload_count: usize,
    fair_demand: u64,
    workload_demand: u64,
    overloaded: bool,
}

#[derive(Debug)]
pub(super) struct RemoteLoadState {
    sequence: AtomicU64,
    runnable: AtomicUsize,
    workload: AtomicUsize,
    fair_demand: AtomicU64,
    workload_demand: AtomicU64,
    incoming_migration_demand: AtomicU64,
    flags: AtomicU16,
    current_primary: AtomicU64,
    current_sequence: AtomicU64,
    pushable_primary: AtomicU64,
    pushable_sequence: AtomicU64,
    fair_balance_armed: AtomicBool,
    fair_balance_deadline_ns: AtomicU64,
}

impl RemoteLoadState {
    pub(super) const fn new() -> Self {
        Self {
            sequence: AtomicU64::new(0),
            runnable: AtomicUsize::new(0),
            workload: AtomicUsize::new(0),
            fair_demand: AtomicU64::new(0),
            workload_demand: AtomicU64::new(0),
            incoming_migration_demand: AtomicU64::new(0),
            flags: AtomicU16::new(0),
            current_primary: AtomicU64::new(0),
            current_sequence: AtomicU64::new(0),
            pushable_primary: AtomicU64::new(0),
            pushable_sequence: AtomicU64::new(0),
            fair_balance_armed: AtomicBool::new(false),
            fair_balance_deadline_ns: AtomicU64::new(0),
        }
    }
}

impl CpuRemote {
    fn publish_load_summary(&self, publication: RunQueueLoadPublication) {
        let RunQueueLoadPublication {
            current_key,
            pushable_key,
            runnable_count,
            workload_count,
            fair_demand,
            workload_demand,
            overloaded,
        } = publication;
        let write_sequence = self.load.sequence.fetch_add(1, Ordering::AcqRel);
        debug_assert_eq!(
            write_sequence & 1,
            0,
            "load summary writers must hold the runqueue lock"
        );
        self.load.runnable.store(runnable_count, Ordering::Relaxed);
        self.load.workload.store(workload_count, Ordering::Relaxed);
        self.load.fair_demand.store(fair_demand, Ordering::Relaxed);
        self.load
            .workload_demand
            .store(workload_demand, Ordering::Relaxed);
        let mut flags = 0;
        if let Some(key) = current_key {
            flags |= SUMMARY_CURRENT_PRESENT;
            flags |=
                (u16::from(key.class_rank()) & SUMMARY_CLASS_MASK) << SUMMARY_CURRENT_CLASS_SHIFT;
            self.load
                .current_primary
                .store(key.primary(), Ordering::Relaxed);
            self.load
                .current_sequence
                .store(key.sequence(), Ordering::Relaxed);
        }
        if let Some(key) = pushable_key {
            flags |= SUMMARY_PUSHABLE_PRESENT;
            flags |=
                (u16::from(key.class_rank()) & SUMMARY_CLASS_MASK) << SUMMARY_PUSHABLE_CLASS_SHIFT;
            self.load
                .pushable_primary
                .store(key.primary(), Ordering::Relaxed);
            self.load
                .pushable_sequence
                .store(key.sequence(), Ordering::Relaxed);
        }
        if overloaded {
            flags |= SUMMARY_OVERLOADED;
        }
        self.load.flags.store(flags, Ordering::Relaxed);
        self.load.sequence.fetch_add(1, Ordering::Release);
    }

    /// Publishes the remotely observable load state while the caller owns this
    /// CPU's runqueue lock.
    ///
    /// Taking the runqueue state by reference keeps queue membership, current
    /// priority, and load publication in one transaction for both owner and
    /// direct remote wake paths.
    pub(crate) fn publish_run_queue_load_summary(&self, run_queue: &CpuRunQueueState) {
        let current = run_queue.current();
        let current_key = current.map(CurrentSchedule::scheduling_key);
        let current_non_idle = current.is_some_and(|current| {
            self.idle_thread()
                .is_none_or(|idle| current.thread() != idle)
        });
        let pushable_key = run_queue.pushable_key();
        let runnable = run_queue.len();
        let workload = runnable.saturating_add(usize::from(current_non_idle));
        let current_fair_demand = current
            .filter(|_| current_non_idle)
            .map_or(0, CurrentSchedule::fair_demand);
        let current_placement_demand = current
            .filter(|_| current_non_idle)
            .map_or(0, CurrentSchedule::placement_demand);
        let fair_demand = run_queue.fair_demand().saturating_add(current_fair_demand);
        let workload_demand = run_queue
            .placement_demand()
            .saturating_add(current_placement_demand);
        self.publish_load_summary(RunQueueLoadPublication {
            current_key,
            pushable_key,
            runnable_count: runnable,
            workload_count: workload,
            fair_demand,
            workload_demand,
            overloaded: pushable_key.is_some() && workload > 1,
        });
    }

    /// Attempts to return a coherent remotely observable scheduling snapshot.
    ///
    /// The owner publishes under a local IRQ guard, but a remote CPU must not
    /// wait indefinitely if that owner is stopped or fails while its sequence
    /// is odd. Callers treat `None` as an unavailable placement candidate and
    /// retry from a later scheduler safe point.
    pub fn try_load_summary(&self) -> Option<CpuLoadSummary> {
        for _ in 0..LOAD_SUMMARY_READ_RETRIES {
            let epoch = self.load.sequence.load(Ordering::Acquire);
            if epoch & 1 != 0 {
                core::hint::spin_loop();
                continue;
            }
            let runnable_count = self.load.runnable.load(Ordering::Relaxed);
            let workload_count = self.load.workload.load(Ordering::Relaxed);
            let fair_demand = self.load.fair_demand.load(Ordering::Relaxed);
            let workload_demand = self.load.workload_demand.load(Ordering::Relaxed);
            let flags = self.load.flags.load(Ordering::Relaxed);
            let current_primary = self.load.current_primary.load(Ordering::Relaxed);
            let current_sequence = self.load.current_sequence.load(Ordering::Relaxed);
            let pushable_primary = self.load.pushable_primary.load(Ordering::Relaxed);
            let pushable_sequence = self.load.pushable_sequence.load(Ordering::Relaxed);
            if self.load.sequence.load(Ordering::Acquire) != epoch {
                continue;
            }
            let current_rank = ((flags >> SUMMARY_CURRENT_CLASS_SHIFT) & SUMMARY_CLASS_MASK) as u8;
            let pushable_rank =
                ((flags >> SUMMARY_PUSHABLE_CLASS_SHIFT) & SUMMARY_CLASS_MASK) as u8;
            return Some(CpuLoadSummary {
                epoch,
                runnable_count,
                workload_count,
                fair_demand,
                workload_demand,
                current_key: (flags & SUMMARY_CURRENT_PRESENT != 0)
                    .then(|| SchedulingKey::new(current_rank, current_primary, current_sequence)),
                pushable_key: (flags & SUMMARY_PUSHABLE_PRESENT != 0).then(|| {
                    SchedulingKey::new(pushable_rank, pushable_primary, pushable_sequence)
                }),
                pushable_class: (flags & SUMMARY_PUSHABLE_PRESENT != 0)
                    .then(|| SchedulingClass::from_rank(pushable_rank)),
                overloaded: flags & SUMMARY_OVERLOADED != 0,
            });
        }
        None
    }

    /// Attempts to return the remotely observable queued runnable count.
    pub fn try_runnable_summary(&self) -> Option<usize> {
        self.try_load_summary().map(CpuLoadSummary::runnable_count)
    }

    pub(crate) fn try_placement_demand(&self) -> Option<u64> {
        self.try_load_summary().map(|summary| {
            summary
                .workload_demand()
                .saturating_add(self.load.incoming_migration_demand.load(Ordering::Acquire))
        })
    }

    pub(super) fn reserve_incoming_migration(&self, demand: u64) {
        if self
            .load
            .incoming_migration_demand
            .try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(demand)
            })
            .is_err()
        {
            task_runtime::fatal_invariant(
                INCOMING_MIGRATION_OVERFLOW_INVARIANT,
                self.owner.as_u32() as usize,
            );
        }
    }

    pub(crate) fn release_incoming_migration_demand(&self, demand: u64) {
        if demand == 0 {
            return;
        }
        let previous_demand = self
            .load
            .incoming_migration_demand
            .fetch_sub(demand, Ordering::AcqRel);
        if previous_demand < demand {
            task_runtime::fatal_invariant(
                INCOMING_MIGRATION_RELEASE_INVARIANT,
                self.owner.as_u32() as usize,
            );
        }
    }

    pub(crate) fn fair_balance_due(&self, now_ns: u64) -> bool {
        self.load.fair_balance_armed.load(Ordering::Acquire)
            && crate::scheduler_time_reached(
                now_ns,
                self.load.fair_balance_deadline_ns.load(Ordering::Relaxed),
            )
    }

    pub(crate) fn defer_fair_balance(&self, now_ns: u64, interval_ns: u64) {
        let interval_ns = interval_ns.max(1);
        assert!(interval_ns < crate::SCHEDULER_TIME_HALF_RANGE);
        self.load.fair_balance_deadline_ns.store(
            crate::scheduler_time_advance(now_ns, interval_ns),
            Ordering::Relaxed,
        );
        self.load.fair_balance_armed.store(true, Ordering::Release);
    }

    pub(in crate::system::cpu) fn fair_balance_deadline_ns(&self) -> Option<u64> {
        self.load
            .fair_balance_armed
            .load(Ordering::Acquire)
            .then(|| self.load.fair_balance_deadline_ns.load(Ordering::Relaxed))
    }

    pub(super) fn reset_fair_balance_for_offline(&self) {
        self.load.fair_balance_armed.store(false, Ordering::Release);
    }

    #[cfg(test)]
    pub(super) fn set_load_summary_sequence_for_test(&self, sequence: u64) {
        self.load.sequence.store(sequence, Ordering::Release);
    }
}
