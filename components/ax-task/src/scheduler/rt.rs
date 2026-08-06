//! Linux-style root-domain and per-runqueue real-time bandwidth state.

#[cfg(test)]
use crate::lock::IrqTicketGuard;
use crate::{
    CpuId, TaskSystemConfig,
    lock::IrqTicketLock,
    runtime::{MonotonicDeadline, MonotonicInstant},
};

/// Linux `rt_rq` runtime ledger protected by the owning rq lock.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RtRunQueueBandwidth {
    enabled: bool,
    runtime_ns: u64,
    time_ns: u64,
    throttled: bool,
}

impl RtRunQueueBandwidth {
    pub(crate) const fn new(period_ns: u64, runtime_ns: u64) -> Self {
        Self {
            enabled: runtime_ns < period_ns,
            runtime_ns,
            time_ns: 0,
            throttled: false,
        }
    }

    /// Accounts current RT execution and reports a raw throttle transition.
    ///
    /// Linux throttles only when `rt_time > rt_runtime`. A PI-boosted owner
    /// does not clear the raw state; it bypasses the effective throttle until
    /// it can release the contended lock.
    pub(crate) fn charge(&mut self, runtime_ns: u64) -> bool {
        if !self.enabled {
            return false;
        }
        let was_throttled = self.throttled;
        self.time_ns = self
            .time_ns
            .checked_add(runtime_ns)
            .expect("one RT period cannot accumulate u64 runtime");
        if self.time_ns > self.runtime_ns {
            self.throttled = true;
        }
        !was_throttled && self.throttled
    }

    pub(crate) const fn may_run(self, boosted: bool) -> bool {
        !self.enabled || boosted || !self.throttled
    }

    pub(crate) const fn is_throttled(self) -> bool {
        self.throttled
    }

    /// Returns time until the strict Linux throttle edge.
    pub(crate) const fn runtime_until_throttle(self) -> Option<u64> {
        if self.throttled || !self.enabled {
            None
        } else {
            Some(self.runtime_ns - self.time_ns + 1)
        }
    }

    /// Applies `overruns` root-period replenishments.
    pub(crate) fn replenish(&mut self, overruns: u64) -> bool {
        let replenishment = (u128::from(self.runtime_ns) * u128::from(overruns))
            .min(u128::from(self.time_ns)) as u64;
        self.time_ns -= replenishment;
        let unthrottled = self.throttled && self.time_ns < self.runtime_ns;
        if unthrottled {
            self.throttled = false;
        }
        unthrottled
    }

    pub(crate) const fn time_ns(self) -> u64 {
        self.time_ns
    }
}

/// One active root-domain RT period callback.
pub(crate) struct RtPeriodFiring {
    generation: u64,
    overruns: u64,
}

impl RtPeriodFiring {
    pub(crate) const fn overruns(&self) -> u64 {
        self.overruns
    }
}

#[derive(Debug)]
struct RootRtBandwidthState {
    owner: Option<CpuId>,
    deadline: Option<MonotonicDeadline>,
    generation: u64,
}

/// The single root-domain hard timer corresponding to Linux `rt_bandwidth`.
#[derive(Debug)]
pub(crate) struct RootRtBandwidth {
    enabled: bool,
    period_ns: u64,
    state: IrqTicketLock<RootRtBandwidthState>,
}

impl RootRtBandwidth {
    pub(crate) fn new(config: TaskSystemConfig) -> Self {
        Self {
            enabled: config.rt_runtime_ns() < config.rt_period_ns(),
            period_ns: config.rt_period_ns(),
            state: IrqTicketLock::new(RootRtBandwidthState {
                owner: None,
                deadline: None,
                generation: 0,
            }),
        }
    }

    /// Starts the root period on the CPU that activated RT work.
    pub(crate) fn activate(&self, cpu: CpuId, now: MonotonicInstant) -> bool {
        if !self.enabled {
            return false;
        }
        let mut state = self.state.lock();
        state.generation = state
            .generation
            .checked_add(1)
            .expect("root RT bandwidth generation exhausted");
        let started = state.deadline.is_none();
        if state.deadline.is_none() {
            state.owner = Some(cpu);
            state.deadline =
                Some(now.deadline_after(core::time::Duration::from_nanos(self.period_ns)));
        }
        started
    }

    pub(crate) fn deadline_for(&self, cpu: CpuId) -> Option<MonotonicDeadline> {
        let state = self.state.lock();
        (state.owner == Some(cpu))
            .then_some(state.deadline)
            .flatten()
    }

    /// Begins one due root-period callback on its pinned owner CPU.
    pub(crate) fn begin_period(&self, cpu: CpuId, now: MonotonicInstant) -> Option<RtPeriodFiring> {
        let mut state = self.state.lock();
        let deadline = state.deadline?;
        if state.owner != Some(cpu) || !now.reached(deadline) {
            return None;
        }
        let elapsed_ns = now.as_nanos() - deadline.as_nanos();
        let overruns = elapsed_ns / self.period_ns + 1;
        let next_ns = (deadline.as_nanos() as u128)
            .checked_add(overruns as u128 * self.period_ns as u128)
            .and_then(|value| u64::try_from(value).ok())
            .and_then(MonotonicDeadline::from_nanos)
            .expect("RT period deadline exceeded the monotonic clock domain");
        state.deadline = Some(next_ns);
        Some(RtPeriodFiring {
            generation: state.generation,
            overruns,
        })
    }

    /// Completes a callback after all online rq ledgers were replenished.
    pub(crate) fn finish_period(&self, firing: RtPeriodFiring, keep_active: bool) {
        let mut state = self.state.lock();
        if state.generation != firing.generation || keep_active {
            return;
        }
        state.owner = None;
        state.deadline = None;
    }

    /// Moves an active pinned period timer away from an offlining CPU.
    pub(crate) fn migrate_owner(&self, offline: CpuId, replacement: CpuId) -> bool {
        let mut state = self.state.lock();
        if state.owner != Some(offline) || state.deadline.is_none() {
            return false;
        }
        state.owner = Some(replacement);
        state.generation = state
            .generation
            .checked_add(1)
            .expect("root RT bandwidth generation exhausted");
        true
    }

    #[cfg(test)]
    fn state(&self) -> IrqTicketGuard<'_, RootRtBandwidthState> {
        self.state.lock()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instant(ns: u64) -> MonotonicInstant {
        MonotonicInstant::from_nanos(ns).unwrap()
    }

    #[test]
    fn strict_runtime_edge_matches_linux_rt() {
        let mut rq = RtRunQueueBandwidth::new(100, 95);

        assert!(!rq.charge(95));
        assert!(rq.may_run(false));
        assert!(rq.charge(1));
        assert!(!rq.may_run(false));
        assert!(rq.may_run(true));
    }

    #[test]
    fn period_unthrottles_only_below_runtime() {
        let mut rq = RtRunQueueBandwidth::new(100, 95);
        rq.charge(191);

        assert!(!rq.replenish(1));
        assert!(rq.is_throttled());
        assert_eq!(rq.time_ns(), 96);
        assert!(rq.replenish(1));
        assert!(!rq.is_throttled());
    }

    #[test]
    fn firing_update_keeps_a_concurrent_activation_live() {
        let root = RootRtBandwidth::new(TaskSystemConfig::new(1));
        root.activate(CpuId::new(0), instant(0));
        let firing = root
            .begin_period(CpuId::new(0), instant(1_000_000_000))
            .unwrap();

        root.activate(CpuId::new(0), instant(1_000_000_000));
        root.finish_period(firing, false);

        assert_eq!(
            root.state().deadline.map(MonotonicDeadline::as_nanos),
            Some(2_000_000_000)
        );
    }

    #[test]
    fn hotplug_migrates_the_single_pinned_period_owner() {
        let root = RootRtBandwidth::new(TaskSystemConfig::new(2));
        assert!(root.activate(CpuId::new(0), instant(0)));

        assert!(root.migrate_owner(CpuId::new(0), CpuId::new(1)));
        assert_eq!(root.deadline_for(CpuId::new(0)), None);
        assert_eq!(
            root.deadline_for(CpuId::new(1))
                .map(MonotonicDeadline::as_nanos),
            Some(1_000_000_000)
        );
        assert!(
            root.begin_period(CpuId::new(0), instant(1_000_000_000))
                .is_none()
        );
        assert_eq!(
            root.begin_period(CpuId::new(1), instant(1_000_000_000))
                .unwrap()
                .overruns(),
            1
        );
    }
}
