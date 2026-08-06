//! Root-domain RT period callback and online-rq replenishment.

use super::*;

impl RootDomain {
    pub(in crate::system::task_system) fn activate_rt_period(
        &self,
        cpu: CpuId,
        now: MonotonicInstant,
    ) -> bool {
        self.rt_bandwidth.activate(cpu, now)
    }

    /// Runs the Linux `do_sched_rt_period_timer()` transaction.
    pub(super) fn service_rt_period(
        &self,
        system: &TaskSystem,
        cpu: CpuId,
        now: MonotonicInstant,
    ) -> bool {
        let Some(firing) = self.rt_bandwidth.begin_period(cpu, now) else {
            return false;
        };
        let overruns = firing.overruns();
        let mut keep_active = false;
        let mut rescheduled = false;

        for remote in &self.runqueues {
            if !remote.is_online() {
                continue;
            }
            let snapshot = *remote.lock_rt_runtime();
            let runnable = remote.lock_run_queue().has_runnable_rt();
            if snapshot.time_ns() == 0 && !runnable {
                continue;
            }
            let transaction = OwnerRqTxn::begin(system, remote);
            if remote.lock_rt_runtime().is_throttled() {
                // Linux balances a throttled rq before subtracting the elapsed
                // periods. A loan obtained here changes the period quantum
                // used by replenishment and can make the rq runnable without
                // waiting for another period edge.
                self.balance_rt_runtime(remote.owner());
            }
            let mut runtime = remote.lock_rt_runtime();
            let unthrottled = runtime.replenish(overruns);
            let runtime_active = runtime.time_ns() != 0 || runtime.is_throttled();
            drop(runtime);
            let runnable = transaction.has_runnable_rt();
            keep_active |= runtime_active || runnable;
            transaction.commit();
            if !unthrottled || !runnable {
                continue;
            }
            rescheduled = true;
            if remote.owner() == cpu {
                remote.request_reschedule();
            } else {
                remote.request_remote_reschedule();
            }
        }

        self.rt_bandwidth.finish_period(firing, keep_active);
        rescheduled
    }

    /// Linux `sched_rt_runtime_exceeded()` plus `do_balance_runtime()`.
    /// The rq caller owns local execution accounting; the root lock is entered
    /// only on the quota edge and serializes transfers among independent
    /// per-rq runtime locks.
    pub(super) fn charge_rt_runtime(&self, cpu: CpuId, runtime_ns: u64) -> bool {
        let remote = &self.runqueues[cpu.as_usize()];
        if !remote.lock_rt_runtime().account(runtime_ns) {
            return false;
        }
        self.balance_rt_runtime(cpu);
        remote.lock_rt_runtime().throttle_if_exceeded()
    }

    fn balance_rt_runtime(&self, receiver: CpuId) {
        let period_ns = self.rt_bandwidth.period_ns();
        let _root = self.rt_bandwidth.lock_runtime();
        let span_weight = self
            .runqueues
            .iter()
            .filter(|remote| remote.is_online())
            .count();
        if span_weight == 0 {
            return;
        }
        for donor in &self.runqueues {
            if donor.owner() == receiver || !donor.is_online() {
                continue;
            }
            let receiver_runtime = self.runqueues[receiver.as_usize()]
                .lock_rt_runtime()
                .runtime_ns();
            let room = period_ns.saturating_sub(receiver_runtime);
            if room == 0 {
                break;
            }
            let amount = {
                let mut runtime = donor.lock_rt_runtime();
                if !runtime.enabled() {
                    continue;
                }
                let amount = (runtime.spare_runtime_ns() / span_weight as u64).min(room);
                if amount != 0 {
                    runtime.lend_runtime(amount);
                }
                amount
            };
            if amount != 0 {
                self.runqueues[receiver.as_usize()]
                    .lock_rt_runtime()
                    .borrow_runtime(amount, period_ns);
            }
            if self.runqueues[receiver.as_usize()]
                .lock_rt_runtime()
                .runtime_ns()
                == period_ns
            {
                break;
            }
        }
    }

    pub(super) fn rt_is_effectively_throttled(&self, cpu: CpuId, has_exempt: bool) -> bool {
        self.runqueues[cpu.as_usize()]
            .lock_rt_runtime()
            .is_throttled()
            && !has_exempt
    }

    pub(in crate::system::task_system) fn enable_rt_runtime(&self, cpu: CpuId) {
        self.runqueues[cpu.as_usize()].lock_rt_runtime().enable(
            self.rt_bandwidth.period_ns(),
            self.rt_bandwidth.runtime_ns(),
        );
    }

    pub(in crate::system::task_system) fn disable_rt_runtime(&self, cpu: CpuId) {
        let _root = self.rt_bandwidth.lock_runtime();
        let base = self.rt_bandwidth.runtime_ns();
        let current = self.runqueues[cpu.as_usize()]
            .lock_rt_runtime()
            .runtime_ns();
        let mut want = i128::from(base) - i128::from(current);
        for remote in &self.runqueues {
            if remote.owner() == cpu || !remote.is_online() || want == 0 {
                continue;
            }
            let mut runtime = remote.lock_rt_runtime();
            if !runtime.enabled() {
                continue;
            }
            if want > 0 {
                let reclaim = u64::try_from(want)
                    .expect("positive RT reclaim must fit u64")
                    .min(runtime.runtime_ns());
                runtime.adjust_runtime(-i128::from(reclaim));
                want -= i128::from(reclaim);
            } else {
                let returned = u64::try_from(-want).expect("negative RT reclaim must fit u64");
                runtime.adjust_runtime(i128::from(returned));
                want = 0;
            }
        }
        assert_eq!(want, 0, "root-domain RT runtime loan leaked across hotplug");
        self.runqueues[cpu.as_usize()].lock_rt_runtime().disable();
    }
}

impl TaskSystem {
    pub(crate) fn service_rt_period(&self, cpu: &CpuLocal, now: MonotonicInstant) -> bool {
        self.root_domain.service_rt_period(self, cpu.owner(), now)
    }

    pub(crate) fn charge_rt_runtime(&self, cpu: CpuId, runtime_ns: u64) -> bool {
        self.root_domain.charge_rt_runtime(cpu, runtime_ns)
    }

    pub(crate) fn rt_is_effectively_throttled(&self, cpu: CpuId, has_exempt: bool) -> bool {
        self.root_domain
            .rt_is_effectively_throttled(cpu, has_exempt)
    }
}
