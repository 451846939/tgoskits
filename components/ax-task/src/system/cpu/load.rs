use super::*;

/// Scheduler class carried by a remotely observed CPU load summary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum SchedulingClass {
    /// Runtime-owned per-CPU stopper work.
    Stop     = 0,
    /// Absolute-deadline EDF work.
    Deadline = 1,
    /// Fixed-priority FIFO or round-robin work.
    Realtime = 2,
    /// Normal or batch EEVDF work.
    Fair     = 3,
    /// Lowest-priority fair idle work.
    Idle     = 4,
}

impl SchedulingClass {
    pub(super) const fn from_rank(rank: u8) -> Self {
        match rank {
            0 => Self::Stop,
            1 => Self::Deadline,
            2 => Self::Realtime,
            3 => Self::Fair,
            _ => Self::Idle,
        }
    }
}

/// Coherent, allocation-free snapshot used by remote placement and balancing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CpuLoadSummary {
    pub(super) epoch: u64,
    pub(super) runnable_count: usize,
    pub(super) workload_count: usize,
    pub(super) fair_demand: u64,
    pub(super) workload_demand: u64,
    pub(super) current_key: Option<SchedulingKey>,
    pub(super) pushable_key: Option<SchedulingKey>,
    pub(super) pushable_class: Option<SchedulingClass>,
    pub(super) overloaded: bool,
}

/// Per-runqueue GRUB utilization snapshot in billionths of one CPU.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeadlineBandwidthSnapshot {
    pub(super) this_bw_scaled: u64,
    pub(super) running_bw_scaled: u64,
    pub(super) max_bw_scaled: u64,
}

impl DeadlineBandwidthSnapshot {
    /// Returns all Deadline utilization assigned to this runqueue.
    pub const fn this_bw_scaled(self) -> u64 {
        self.this_bw_scaled
    }

    /// Returns ActiveContending plus ActiveNonContending utilization.
    pub const fn running_bw_scaled(self) -> u64 {
        self.running_bw_scaled
    }

    /// Returns utilization currently eligible for GRUB reclaim.
    pub const fn inactive_bw_scaled(self) -> u64 {
        assert!(self.running_bw_scaled <= self.this_bw_scaled);
        self.this_bw_scaled - self.running_bw_scaled
    }

    /// Returns the per-CPU reclaim capacity.
    pub const fn max_bw_scaled(self) -> u64 {
        self.max_bw_scaled
    }
}

impl CpuLoadSummary {
    /// Returns the publication epoch read with this coherent snapshot.
    pub const fn epoch(self) -> u64 {
        self.epoch
    }

    /// Returns queued non-idle work owned by this CPU.
    pub const fn runnable_count(self) -> usize {
        self.runnable_count
    }

    /// Returns queued work plus the currently running non-idle thread.
    pub const fn workload_count(self) -> usize {
        self.workload_count
    }

    /// Returns the Linux nice-weighted Fair demand owned by this CPU.
    pub const fn fair_demand(self) -> u64 {
        self.fair_demand
    }

    /// Returns instantaneous scheduling demand in normal-nice weight units.
    ///
    /// Fair work contributes its exact nice weight. RT and Deadline work each
    /// contribute one normal-nice capacity unit until class-specific
    /// utilization tracking is available.
    pub const fn workload_demand(self) -> u64 {
        self.workload_demand
    }

    /// Returns the effective urgency of the current dispatch, including PI.
    pub const fn current_key(self) -> Option<SchedulingKey> {
        self.current_key
    }

    /// Returns the most urgent queued candidate that can leave this CPU.
    pub const fn pushable_key(self) -> Option<SchedulingKey> {
        self.pushable_key
    }

    /// Returns the scheduler class of the top pushable candidate.
    pub const fn pushable_class(self) -> Option<SchedulingClass> {
        self.pushable_class
    }

    /// Reports whether this CPU owns more runnable work than it can execute.
    pub const fn is_overloaded(self) -> bool {
        self.overloaded
    }
}

pub(super) const SUMMARY_CURRENT_PRESENT: u16 = 1 << 0;
pub(super) const SUMMARY_PUSHABLE_PRESENT: u16 = 1 << 1;
pub(super) const SUMMARY_OVERLOADED: u16 = 1 << 2;
pub(super) const SUMMARY_CURRENT_CLASS_SHIFT: u32 = 3;
pub(super) const SUMMARY_PUSHABLE_CLASS_SHIFT: u32 = 6;
pub(super) const SUMMARY_CLASS_MASK: u16 = 0b111;
pub(super) const LOAD_SUMMARY_READ_RETRIES: usize = 8;
