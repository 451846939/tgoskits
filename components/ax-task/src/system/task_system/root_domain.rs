//! Root-domain topology, priority indexes, and Deadline bandwidth ownership.

use core::{
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
};

use super::*;
use crate::{DEADLINE_UTILIZATION_SCALE, RtPriority, lock::PreemptTicketGuard};

/// The scheduler-wide owner corresponding to Linux `struct root_domain`.
///
/// Runqueues remain the physical owner of runnable entities and local
/// `this_bw`/`running_bw`. This object owns facts shared by those runqueues:
/// online topology, Deadline admission, cpupri/cpudl indexes, and the spare
/// bandwidth that GRUB distributes across the online domain.
#[derive(Debug)]
pub(super) struct RootDomain {
    state: PreemptTicketLock<RootDomainState>,
    priority: RootDomainPriorityIndex,
    deadline_extra_bw_scaled: AtomicU64,
    online_cpus: AtomicUsize,
    pending_deadline_releases: AtomicU64,
    deadline_max_bw_scaled: u64,
}

#[derive(Debug)]
pub(super) struct RootDomainState {
    pub(super) online: CpuSet,
    pub(super) deadline_admission: DeadlineAdmission,
}

impl RootDomainState {
    pub(super) fn reserve_deadline(
        &mut self,
        policy: SchedulePolicy,
        affinity: &CpuSet,
    ) -> Result<u128, TaskError> {
        match policy {
            SchedulePolicy::Deadline(deadline) => {
                if !affinity.covers(&self.online) {
                    return Err(TaskError::DeadlineAffinity);
                }
                self.deadline_admission.reserve(deadline)
            }
            _ => Ok(0),
        }
    }

    pub(super) fn deadline_reservation_for(
        &self,
        policy: SchedulePolicy,
        affinity: &CpuSet,
    ) -> Result<u128, TaskError> {
        match policy {
            SchedulePolicy::Deadline(deadline) => {
                if !affinity.covers(&self.online) {
                    return Err(TaskError::DeadlineAffinity);
                }
                Ok(DeadlineAdmission::utilization(deadline))
            }
            _ => Ok(0),
        }
    }

    pub(super) fn admission_overcommitted(&self) -> bool {
        self.deadline_admission.reserved_scaled() > self.deadline_admission.capacity_scaled()
    }
}

pub(super) struct RootDomainGuard<'domain> {
    owner: &'domain RootDomain,
    state: PreemptTicketGuard<'domain, RootDomainState>,
}

impl RootDomain {
    pub(super) fn new(config: TaskSystemConfig) -> Self {
        let deadline_max_bw_scaled =
            u64::from(config.deadline_cap_percent()) * DEADLINE_UTILIZATION_SCALE / 100;
        Self {
            state: PreemptTicketLock::new(RootDomainState {
                online: CpuSet::empty(config.cpu_count()),
                deadline_admission: DeadlineAdmission::new(config.deadline_cap_percent()),
            }),
            priority: RootDomainPriorityIndex::new(config.cpu_count()),
            deadline_extra_bw_scaled: AtomicU64::new(0),
            online_cpus: AtomicUsize::new(0),
            pending_deadline_releases: AtomicU64::new(0),
            deadline_max_bw_scaled,
        }
    }

    pub(super) fn lock(&self) -> RootDomainGuard<'_> {
        let mut state = self.state.lock();
        let released = self.pending_deadline_releases.swap(0, Ordering::AcqRel);
        state.deadline_admission.release(u128::from(released));
        RootDomainGuard { owner: self, state }
    }

    pub(super) fn defer_deadline_release(&self, released: u64) -> Result<(), TaskError> {
        self.pending_deadline_releases
            .try_update(Ordering::AcqRel, Ordering::Acquire, |pending| {
                pending.checked_add(released)
            })
            .map(|_| ())
            .map_err(|_| TaskError::InvalidConfiguration)
    }

    pub(super) fn deadline_extra_bw_scaled(&self) -> u64 {
        let online_cpus = self.online_cpus.load(Ordering::Acquire);
        if online_cpus == 0 {
            return 0;
        }
        let base = self.deadline_extra_bw_scaled.load(Ordering::Acquire);
        let pending = self.pending_deadline_releases.load(Ordering::Acquire);
        base.saturating_add(pending / online_cpus as u64)
            .min(self.deadline_max_bw_scaled)
    }

    pub(super) fn publish_run_queue(&self, cpu: CpuId, run_queue: &CpuRunQueueState, online: bool) {
        self.priority.publish_run_queue(cpu, run_queue, online);
    }

    pub(super) fn publish_offline(&self, cpu: CpuId) {
        self.priority.publish_offline(cpu);
    }

    pub(super) fn find_lowest_rt_cpu(
        &self,
        priority: RtPriority,
        affinity: &CpuSet,
        preferred: Option<CpuId>,
        accepts: impl FnMut(CpuId) -> bool,
    ) -> Option<CpuId> {
        self.priority
            .find_lowest_rt_cpu(priority, affinity, preferred, accepts)
    }

    pub(super) fn find_later_deadline_cpu(
        &self,
        absolute_deadline_ns: u64,
        affinity: &CpuSet,
        preferred: Option<CpuId>,
        accepts: impl FnMut(CpuId) -> bool,
    ) -> Option<CpuId> {
        self.priority
            .find_later_deadline_cpu(absolute_deadline_ns, affinity, preferred, accepts)
    }

    fn publish_deadline_bandwidth(&self, state: &mut RootDomainState) {
        let online_cpus = state.online.count();
        state.deadline_admission.set_online_cpus(online_cpus);
        let reserved_per_cpu = if online_cpus == 0 {
            self.deadline_max_bw_scaled
        } else {
            let scaled = state.deadline_admission.reserved_scaled() / online_cpus as u128;
            u64::try_from(scaled).unwrap_or(u64::MAX)
        };
        let extra = self
            .deadline_max_bw_scaled
            .saturating_sub(reserved_per_cpu.min(self.deadline_max_bw_scaled));
        self.deadline_extra_bw_scaled
            .store(extra, Ordering::Release);
        self.online_cpus.store(online_cpus, Ordering::Release);
    }
}

impl Deref for RootDomainGuard<'_> {
    type Target = RootDomainState;

    fn deref(&self) -> &Self::Target {
        &self.state
    }
}

impl DerefMut for RootDomainGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.state
    }
}

impl Drop for RootDomainGuard<'_> {
    fn drop(&mut self) {
        self.owner.publish_deadline_bandwidth(&mut self.state);
    }
}
