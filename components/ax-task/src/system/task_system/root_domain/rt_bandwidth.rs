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
    pub(super) fn service_rt_period(&self, cpu: CpuId, now: MonotonicInstant) -> bool {
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
            let (unthrottled, runnable) = {
                let mut run_queue = remote.lock_run_queue();
                let unthrottled = run_queue.replenish_rt_runtime(overruns);
                let runnable = run_queue.has_runnable_rt();
                keep_active |= run_queue.has_rt_activity();
                (unthrottled, runnable)
            };
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
}

impl TaskSystem {
    pub(crate) fn service_rt_period(&self, cpu: &CpuLocal, now: MonotonicInstant) -> bool {
        self.root_domain.service_rt_period(cpu.owner(), now)
    }
}
