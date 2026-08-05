//! Runqueue-owned scheduler clock state.

use crate::SchedulerTimestamp;

/// One scheduler-clock sample accepted under the owning runqueue lock.
///
/// Construction remains private to [`RunQueueClock`], so scheduler internals
/// cannot substitute a caller-provided timestamp for the target runqueue's
/// authoritative clock.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RunQueueClockSnapshot {
    now: SchedulerTimestamp,
}

impl RunQueueClockSnapshot {
    pub(crate) const fn as_nanos(self) -> u64 {
        self.now.as_nanos()
    }
}

/// Cached scheduler clock serialized by one CPU runqueue lock.
///
/// This mirrors Linux `rq->clock`: the first source sample initializes the
/// cache, later forward samples advance it, and a negative signed delta is
/// ignored. The wrapping comparison keeps a real counter wrap moving forward.
#[derive(Debug)]
pub(crate) struct RunQueueClock {
    now: Option<SchedulerTimestamp>,
}

impl RunQueueClock {
    pub(crate) const fn new() -> Self {
        Self { now: None }
    }

    pub(crate) fn update(&mut self, source: SchedulerTimestamp) -> RunQueueClockSnapshot {
        let now = match self.now {
            Some(now) if source.is_before(now) => now,
            Some(_) | None => source,
        };
        self.now = Some(now);
        RunQueueClockSnapshot { now }
    }

    /// Returns the last sample accepted by the runqueue owner.
    ///
    /// Like Linux `rq_clock()`, this accessor never reads the architecture
    /// clock source. The caller must already have updated the runqueue clock in
    /// the surrounding owner transaction.
    pub(crate) fn snapshot(&self) -> Option<RunQueueClockSnapshot> {
        self.now.map(|now| RunQueueClockSnapshot { now })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negative_source_delta_does_not_move_the_runqueue_clock_backwards() {
        let mut clock = RunQueueClock::new();

        assert_eq!(
            clock.update(SchedulerTimestamp::from_nanos(100)).as_nanos(),
            100
        );
        assert_eq!(
            clock.update(SchedulerTimestamp::from_nanos(90)).as_nanos(),
            100
        );
    }

    #[test]
    fn scheduler_counter_wrap_advances_the_runqueue_clock() {
        let mut clock = RunQueueClock::new();

        clock.update(SchedulerTimestamp::from_nanos(u64::MAX - 2));

        assert_eq!(
            clock.update(SchedulerTimestamp::from_nanos(2)).as_nanos(),
            2
        );
    }
}
