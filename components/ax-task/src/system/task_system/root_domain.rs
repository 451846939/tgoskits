//! Root-domain topology, priority indexes, and Deadline bandwidth ownership.

use core::ops::Deref;

use super::*;
use crate::{DEADLINE_UTILIZATION_SCALE, RtPriority, lock::PreemptTicketGuard};

/// The scheduler-wide owner corresponding to Linux `struct root_domain`.
///
/// Runqueues remain the physical owner of runnable entities and local
/// `this_bw`/`running_bw`. This object owns facts shared by those runqueues:
/// online topology, Deadline admission, and cpupri/cpudl indexes. Every
/// runqueue stores its own published
/// `extra_bw`, matching Linux `dl_rq`, while this object owns the root-domain
/// total used to derive those values.
#[derive(Debug)]
pub(super) struct RootDomain {
    state: PreemptTicketLock<RootDomainState>,
    priority: RootDomainPriorityIndex,
    runqueues: Vec<Arc<CpuRemote>>,
    deadline_max_bw_scaled: u64,
}

#[derive(Debug)]
pub(super) struct RootDomainState {
    pub(super) online: CpuSet,
    deadline_admission: DeadlineAdmission,
}

pub(super) struct RootDomainGuard<'domain> {
    owner: &'domain RootDomain,
    state: PreemptTicketGuard<'domain, RootDomainState>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct DeadlineBandwidthRebuild {
    pub(super) online_cpus: u32,
    pub(super) reserved_scaled: u64,
    pub(super) distributed_scaled: u64,
}

impl RootDomain {
    pub(super) fn new(config: TaskSystemConfig, runqueues: Vec<Arc<CpuRemote>>) -> Self {
        debug_assert_eq!(config.cpu_count(), runqueues.len());
        let deadline_max_bw_scaled =
            u64::from(config.deadline_cap_percent()) * DEADLINE_UTILIZATION_SCALE / 100;
        Self {
            state: PreemptTicketLock::new(RootDomainState {
                online: CpuSet::empty(config.cpu_count()),
                deadline_admission: DeadlineAdmission::new(config.deadline_cap_percent()),
            }),
            priority: RootDomainPriorityIndex::new(config.cpu_count()),
            runqueues,
            deadline_max_bw_scaled,
        }
    }

    pub(super) fn lock(&self) -> RootDomainGuard<'_> {
        RootDomainGuard {
            owner: self,
            state: self.state.lock(),
        }
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

    fn rebuild_deadline_bandwidth(
        &self,
        state: &mut RootDomainState,
        rebuild: DeadlineBandwidthRebuild,
    ) {
        assert_eq!(
            state.online.count(),
            rebuild.online_cpus as usize,
            "Deadline rebuild topology must match the root-domain mask"
        );
        assert_eq!(
            state.deadline_admission.reserved_scaled(),
            rebuild.reserved_scaled,
            "Deadline rebuild must account every admitted reservation"
        );
        state
            .deadline_admission
            .set_online_cpus(rebuild.online_cpus);
        assert!(
            rebuild.distributed_scaled <= self.deadline_max_bw_scaled,
            "admission must reject root-domain Deadline overcommit before publication"
        );
        let extra = self.deadline_max_bw_scaled - rebuild.distributed_scaled;
        for remote in &self.runqueues {
            let published = if state.online.contains(remote.owner()) {
                extra
            } else {
                self.deadline_max_bw_scaled
            };
            remote.publish_deadline_extra_bw(published);
        }
    }

    fn replace_deadline_bandwidth(
        &self,
        state: &RootDomainState,
        old_utilization: u64,
        new_utilization: u64,
    ) {
        let online_cpus = u64::try_from(state.online.count())
            .expect("validated root-domain topology must fit CpuId");
        assert_ne!(
            online_cpus, 0,
            "Deadline admission requires an online root-domain CPU"
        );
        let old_per_cpu = old_utilization / online_cpus;
        let new_per_cpu = new_utilization / online_cpus;
        for remote in &self.runqueues {
            if state.online.contains(remote.owner()) {
                let extra = remote
                    .deadline_extra_bw_scaled()
                    .checked_add(old_per_cpu)
                    .expect("dl_rq extra bandwidth must fit its fixed-point ledger")
                    .checked_sub(new_per_cpu)
                    .expect("admission must not consume unavailable dl_rq extra bandwidth");
                assert!(
                    extra <= self.deadline_max_bw_scaled,
                    "Deadline replacement must match a previously published reservation"
                );
                remote.publish_deadline_extra_bw(extra);
            }
        }
    }
}

impl RootDomainGuard<'_> {
    pub(super) fn reserve_deadline(
        &mut self,
        policy: SchedulePolicy,
        affinity: &CpuSet,
    ) -> Result<u64, TaskError> {
        let reservation = self.deadline_reservation_for(policy, affinity)?;
        if reservation != 0 {
            self.state
                .deadline_admission
                .reserve_utilization(reservation)?;
            self.owner
                .replace_deadline_bandwidth(&self.state, 0, reservation);
        }
        Ok(reservation)
    }

    pub(super) fn deadline_reservation_for(
        &self,
        policy: SchedulePolicy,
        affinity: &CpuSet,
    ) -> Result<u64, TaskError> {
        match policy {
            SchedulePolicy::Deadline(deadline) => {
                if !affinity.covers(&self.state.online) {
                    return Err(TaskError::DeadlineAffinity);
                }
                Ok(DeadlineAdmission::utilization(deadline))
            }
            _ => Ok(0),
        }
    }

    pub(super) fn replace_deadline_utilization(
        &mut self,
        old_utilization: u64,
        new_utilization: u64,
    ) -> Result<(), TaskError> {
        if old_utilization == new_utilization {
            return Ok(());
        }
        self.state
            .deadline_admission
            .replace_utilization(old_utilization, new_utilization)?;
        self.owner
            .replace_deadline_bandwidth(&self.state, old_utilization, new_utilization);
        Ok(())
    }

    pub(super) fn release_deadline(&mut self, utilization: u64) {
        if utilization == 0 {
            return;
        }
        self.replace_deadline_utilization(utilization, 0)
            .expect("root-domain Deadline release must match an admitted reservation");
    }

    pub(super) fn admission_overcommitted(&self) -> bool {
        self.state.deadline_admission.reserved_scaled()
            > self.state.deadline_admission.capacity_scaled()
    }

    pub(super) fn can_deactivate_cpu(&self, cpu: CpuId) -> bool {
        if !self.state.online.contains(cpu) {
            return false;
        }
        let remaining = self.state.online.count() - 1;
        let remaining =
            u64::try_from(remaining).expect("validated root-domain topology must fit CpuId");
        let capacity = remaining * self.owner.deadline_max_bw_scaled;
        self.state.deadline_admission.reserved_scaled() <= capacity
    }

    pub(super) fn insert_online(&mut self, cpu: CpuId, rebuild: DeadlineBandwidthRebuild) -> bool {
        if !self.state.online.insert(cpu) {
            return false;
        }
        self.owner
            .rebuild_deadline_bandwidth(&mut self.state, rebuild);
        true
    }

    pub(super) fn remove_online(&mut self, cpu: CpuId, rebuild: DeadlineBandwidthRebuild) -> bool {
        if !self.state.online.remove(cpu) {
            return false;
        }
        self.owner
            .rebuild_deadline_bandwidth(&mut self.state, rebuild);
        true
    }
}

impl Deref for RootDomainGuard<'_> {
    type Target = RootDomainState;

    fn deref(&self) -> &Self::Target {
        &self.state
    }
}
