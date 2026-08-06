//! Linux GRUB zero-lag and reclaim accounting semantics.

use ax_task::{
    CpuId, DeadlineActivity, DeadlineFlags, DeadlinePolicy, FairMode, Nice, SchedulePolicy,
    TaskError, TaskSystem, TaskSystemConfig, ThreadSpec, ThreadState, WakeResult,
};

pub mod support;
use support::TaskSystemClockTestExt;

#[test]
fn reclaim_includes_unreserved_root_domain_bandwidth() {
    let (system, mut cpu) = online_system();
    let reclaimer = ready_deadline(&system, 500, 1_000, 1_000, DeadlineFlags::RECLAIM);
    assert_eq!(
        cpu.deadline_extra_bw_scaled(),
        450_000_000,
        "the owner runqueue must receive its Linux dl_rq.extra_bw share"
    );
    system.enqueue_at(cpu.as_mut(), reclaimer.id(), 0).unwrap();
    assert_eq!(
        system.schedule_at(cpu.as_mut(), 0).unwrap().next(),
        reclaimer.id()
    );

    assert!(
        !system
            .charge_current_at(cpu.as_mut(), 100, 100, 0)
            .unwrap()
            .slice_expired()
    );
    assert_eq!(
        system.schedule_at(cpu.as_mut(), 100).unwrap().next(),
        reclaimer.id()
    );
    assert_eq!(
        system
            .deadline_runtime(reclaimer.id())
            .unwrap()
            .remaining_runtime_ns(),
        448,
        "GRUB must include the root domain's unreserved capacity in Uextra"
    );
}

#[test]
fn detached_policy_release_updates_runqueue_extra_bandwidth_synchronously() {
    let (system, cpu) = online_system();
    let deadline = system
        .create_thread(ThreadSpec::new(SchedulePolicy::deadline(
            DeadlinePolicy::new(1, 2, 2, DeadlineFlags::NONE).unwrap(),
        )))
        .unwrap();
    assert_eq!(cpu.deadline_extra_bw_scaled(), 450_000_000);

    system
        .set_thread_policy(deadline.id(), SchedulePolicy::default())
        .unwrap();
    assert_eq!(
        cpu.deadline_extra_bw_scaled(),
        950_000_000,
        "policy apply must release dl_bw before returning, not through a pending fallback"
    );
}

#[test]
fn root_domain_extra_bandwidth_uses_linux_per_reservation_rounding() {
    support::clear_handles();
    let system = TaskSystem::new(TaskSystemConfig::new(2)).unwrap();
    let mut cpu0 = system.create_cpu_local(CpuId::new(0)).unwrap();
    let mut cpu1 = system.create_cpu_local(CpuId::new(1)).unwrap();
    for cpu in [&mut cpu0, &mut cpu1] {
        system
            .register_idle_thread(
                cpu.as_mut(),
                ThreadSpec::new(SchedulePolicy::fair(Nice::ZERO, FairMode::Idle)),
            )
            .unwrap();
        system.bring_cpu_online(cpu.as_mut()).unwrap();
    }
    let policy = SchedulePolicy::deadline(
        DeadlinePolicy::new(1, 1_000_000_000, 1_000_000_000, DeadlineFlags::NONE).unwrap(),
    );
    system.create_thread(ThreadSpec::new(policy)).unwrap();
    system.create_thread(ThreadSpec::new(policy)).unwrap();

    assert_eq!(cpu0.deadline_extra_bw_scaled(), 950_000_000);
    assert_eq!(cpu1.deadline_extra_bw_scaled(), 950_000_000);
    system.take_cpu_offline(cpu1.as_mut()).unwrap();
    assert_eq!(cpu0.deadline_extra_bw_scaled(), 949_999_998);
    system.bring_cpu_online(cpu1.as_mut()).unwrap();
    assert_eq!(cpu0.deadline_extra_bw_scaled(), 950_000_000);
    assert_eq!(cpu1.deadline_extra_bw_scaled(), 950_000_000);
}

#[test]
fn deadline_policy_replace_rounds_old_and_new_reservations_separately() {
    support::clear_handles();
    let system = TaskSystem::new(TaskSystemConfig::new(2)).unwrap();
    let mut cpu0 = system.create_cpu_local(CpuId::new(0)).unwrap();
    let mut cpu1 = system.create_cpu_local(CpuId::new(1)).unwrap();
    for cpu in [&mut cpu0, &mut cpu1] {
        system
            .register_idle_thread(
                cpu.as_mut(),
                ThreadSpec::new(SchedulePolicy::fair(Nice::ZERO, FairMode::Idle)),
            )
            .unwrap();
        system.bring_cpu_online(cpu.as_mut()).unwrap();
    }
    let two = SchedulePolicy::deadline(
        DeadlinePolicy::new(2, 1_000_000_000, 1_000_000_000, DeadlineFlags::NONE).unwrap(),
    );
    let thread = ready_deadline(
        &system,
        1,
        1_000_000_000,
        1_000_000_000,
        DeadlineFlags::NONE,
    );
    system.enqueue_at(cpu0.as_mut(), thread.id(), 0).unwrap();

    system.set_thread_policy(thread.id(), two).unwrap();

    assert_eq!(cpu0.deadline_extra_bw_scaled(), 949_999_999);
    assert_eq!(cpu1.deadline_extra_bw_scaled(), 949_999_999);
    system.drain_policy_updates_at(cpu0.as_mut(), 0).unwrap();
}

#[test]
fn reclaim_starts_only_after_the_blocked_reservation_zero_lag_time() {
    let (system, mut cpu) = online_system();
    let donor = ready_deadline(&system, 4, 8, 8, DeadlineFlags::NONE);
    let reclaimer = ready_deadline(&system, 4, 8, 16, DeadlineFlags::RECLAIM);
    system.enqueue_at(cpu.as_mut(), donor.id(), 0).unwrap();
    system.enqueue_at(cpu.as_mut(), reclaimer.id(), 0).unwrap();
    assert_eq!(
        system.schedule_at(cpu.as_mut(), 0).unwrap().next(),
        donor.id()
    );
    assert!(
        !system
            .charge_current_at(cpu.as_mut(), 2, 2, 0)
            .unwrap()
            .slice_expired()
    );

    support::set_monotonic_ns(2);
    assert_eq!(
        system.block_current_at(cpu.as_mut(), 2).unwrap().next(),
        reclaimer.id()
    );
    // The donor has q=2 and d=8, so zero-lag is 8 - 2*8/4 = 4.
    let activity = system.deadline_activity(donor.id()).unwrap();
    assert_eq!(activity.activity(), DeadlineActivity::ActiveNonContending);
    assert_eq!(activity.zero_lag_ns(), Some(4));
    assert_eq!(cpu.deadline_bandwidth().this_bw_scaled(), 750_000_000);
    assert_eq!(cpu.deadline_bandwidth().running_bw_scaled(), 750_000_000);
    support::set_monotonic_ns(4);
    let irq_budget = cpu.batch_limit();
    assert_eq!(
        cpu.as_mut()
            .on_task_clock_event(
                ax_task::runtime::MonotonicInstant::from_nanos(4).unwrap(),
                irq_budget,
            )
            .expired(),
        1
    );
    assert!(
        system
            .schedule_if_requested_at(cpu.as_mut(), 4)
            .unwrap()
            .decision()
            .is_none()
    );
    let activity = system.deadline_activity(donor.id()).unwrap();
    assert_eq!(activity.activity(), DeadlineActivity::Inactive);
    assert_eq!(activity.zero_lag_ns(), None);
    assert_eq!(cpu.deadline_bandwidth().inactive_bw_scaled(), 500_000_000);
    assert_eq!(
        system
            .deadline_runtime(reclaimer.id())
            .unwrap()
            .remaining_runtime_ns(),
        3
    );

    assert!(
        !system
            .charge_current_at(cpu.as_mut(), 6, 2, 0)
            .unwrap()
            .slice_expired()
    );
    assert_eq!(
        system.schedule_at(cpu.as_mut(), 6).unwrap().next(),
        reclaimer.id()
    );
    // Umax=.95, Uinactive=.5 and Ui=.25: Linux's fixed-point GRUB rule
    // truncates this two-nanosecond charge to zero.
    assert_eq!(
        system
            .deadline_runtime(reclaimer.id())
            .unwrap()
            .remaining_runtime_ns(),
        3
    );
}

#[test]
fn deadline_yield_does_not_publish_immediate_reclaimable_runtime() {
    let (system, mut cpu) = online_system();
    let donor = ready_deadline(&system, 4, 8, 8, DeadlineFlags::NONE);
    let reclaimer = ready_deadline(&system, 4, 8, 16, DeadlineFlags::RECLAIM);
    system.enqueue_at(cpu.as_mut(), donor.id(), 0).unwrap();
    system.enqueue_at(cpu.as_mut(), reclaimer.id(), 0).unwrap();
    assert_eq!(
        system.schedule_at(cpu.as_mut(), 0).unwrap().next(),
        donor.id()
    );
    system.charge_current_at(cpu.as_mut(), 2, 2, 0).unwrap();

    support::set_monotonic_ns(2);
    assert_eq!(
        system.yield_current_at(cpu.as_mut(), 2).unwrap().next(),
        reclaimer.id()
    );
    let activity = system.deadline_activity(donor.id()).unwrap();
    assert_eq!(activity.activity(), DeadlineActivity::ActiveNonContending);
    assert_eq!(activity.zero_lag_ns(), Some(8));
    assert_eq!(cpu.deadline_bandwidth().inactive_bw_scaled(), 0);
    assert!(
        !system
            .charge_current_at(cpu.as_mut(), 6, 4, 0)
            .unwrap()
            .slice_expired(),
        "root-domain Uextra remains reclaimable before zero-lag"
    );
    assert_eq!(
        system.schedule_at(cpu.as_mut(), 6).unwrap().next(),
        reclaimer.id()
    );
    assert_eq!(
        system
            .deadline_runtime(reclaimer.id())
            .unwrap()
            .remaining_runtime_ns(),
        1,
        "the yielded donor must not contribute Uinactive before zero-lag"
    );
}

#[test]
fn wake_before_zero_lag_cancels_the_pending_inactive_transition() {
    let (system, mut cpu) = online_system();
    let thread = ready_deadline(&system, 4, 8, 8, DeadlineFlags::NONE);
    system.enqueue_at(cpu.as_mut(), thread.id(), 0).unwrap();
    assert_eq!(
        system.schedule_at(cpu.as_mut(), 0).unwrap().next(),
        thread.id()
    );
    system.charge_current_at(cpu.as_mut(), 2, 2, 0).unwrap();
    support::set_monotonic_ns(2);
    system.block_current_at(cpu.as_mut(), 2).unwrap();
    system.complete_context_switch(cpu.as_mut()).unwrap();

    install_runtime_handles(&system, cpu.as_mut());
    assert_eq!(thread.wake_handle().wake(), WakeResult::Notified);
    let activity = system.deadline_activity(thread.id()).unwrap();
    assert_eq!(
        activity.activity(),
        DeadlineActivity::ActiveContending,
        "remote wake must account CBS activity in the same target-rq transaction"
    );
    assert_eq!(activity.zero_lag_ns(), None);
    assert_eq!(cpu.deadline_bandwidth().inactive_bw_scaled(), 0);
    system.drain_policy_updates_at(cpu.as_mut(), 3).unwrap();
    let activity = system.deadline_activity(thread.id()).unwrap();
    assert_eq!(activity.activity(), DeadlineActivity::ActiveContending);
    assert_eq!(activity.zero_lag_ns(), None);
    assert_eq!(cpu.deadline_bandwidth().inactive_bw_scaled(), 0);

    assert_eq!(
        system.schedule_at(cpu.as_mut(), 4).unwrap().next(),
        thread.id()
    );
    assert_eq!(
        system.deadline_activity(thread.id()).unwrap().activity(),
        DeadlineActivity::ActiveContending
    );
    support::clear_handles();
}

#[test]
fn throttled_wake_cannot_restore_cbs_budget_before_replenishment() {
    let (system, mut cpu) = online_system();
    let thread = ready_deadline(&system, 2, 10, 20, DeadlineFlags::NONE);
    system.enqueue_at(cpu.as_mut(), thread.id(), 0).unwrap();
    assert_eq!(
        system.schedule_at(cpu.as_mut(), 0).unwrap().next(),
        thread.id()
    );
    assert!(
        system
            .charge_current_at(cpu.as_mut(), 2, 2, 0)
            .unwrap()
            .slice_expired()
    );
    assert_ne!(
        system.schedule_at(cpu.as_mut(), 2).unwrap().next(),
        thread.id()
    );
    system.complete_context_switch(cpu.as_mut()).unwrap();
    assert_eq!(thread.state(), ThreadState::Blocked);
    assert_eq!(
        system
            .deadline_runtime(thread.id())
            .unwrap()
            .remaining_runtime_ns(),
        0
    );

    install_runtime_handles(&system, cpu.as_mut());
    assert_eq!(thread.wake_handle().wake(), WakeResult::Notified);
    system.drain_policy_updates_at(cpu.as_mut(), 3).unwrap();
    assert_eq!(thread.state(), ThreadState::Blocked);
    assert_eq!(
        system
            .deadline_runtime(thread.id())
            .unwrap()
            .remaining_runtime_ns(),
        0
    );
    support::set_monotonic_ns(9);
    if let Some(decision) = system
        .schedule_if_requested_at(cpu.as_mut(), 9)
        .unwrap()
        .decision()
    {
        assert_ne!(decision.next(), thread.id());
    }
    assert_eq!(thread.state(), ThreadState::Blocked);
    // CBS depletion waits for the next release. For constrained D<P
    // reservations, replenishing at the scheduling deadline would provide a
    // second budget inside the same period.
    support::set_monotonic_ns(10);
    if let Some(decision) = system
        .schedule_if_requested_at(cpu.as_mut(), 10)
        .unwrap()
        .decision()
    {
        assert_ne!(decision.next(), thread.id());
    }
    assert_eq!(thread.state(), ThreadState::Blocked);
    support::set_monotonic_ns(20);
    let irq_budget = cpu.batch_limit();
    assert_eq!(
        cpu.as_mut()
            .on_task_clock_event(
                ax_task::runtime::MonotonicInstant::from_nanos(20).unwrap(),
                irq_budget,
            )
            .expired(),
        1
    );
    let decision = system.schedule_at(cpu.as_mut(), 20).unwrap();
    assert_eq!(decision.next(), thread.id());
    assert_eq!(
        system
            .deadline_runtime(thread.id())
            .unwrap()
            .remaining_runtime_ns(),
        2
    );
    support::clear_handles();
}

#[test]
fn deadline_bandwidth_moves_between_owner_runqueues() {
    support::clear_handles();
    let system = TaskSystem::new(TaskSystemConfig::new(2)).unwrap();
    let mut cpu0 = system.create_cpu_local(CpuId::new(0)).unwrap();
    let mut cpu1 = system.create_cpu_local(CpuId::new(1)).unwrap();
    for cpu in [&mut cpu0, &mut cpu1] {
        system
            .register_idle_thread(
                cpu.as_mut(),
                ThreadSpec::new(SchedulePolicy::fair(Nice::ZERO, FairMode::Idle)),
            )
            .unwrap();
        system.bring_cpu_online(cpu.as_mut()).unwrap();
    }
    let first = ready_deadline(&system, 2, 10, 20, DeadlineFlags::NONE);
    let second = ready_deadline(&system, 2, 10, 20, DeadlineFlags::NONE);
    assert_eq!(cpu0.deadline_extra_bw_scaled(), 850_000_000);
    assert_eq!(cpu1.deadline_extra_bw_scaled(), 850_000_000);
    system.enqueue_at(cpu0.as_mut(), first.id(), 0).unwrap();
    system.enqueue_at(cpu0.as_mut(), second.id(), 0).unwrap();
    assert_eq!(cpu0.deadline_bandwidth().this_bw_scaled(), 200_000_000);

    let migrated = system
        .push_overloaded(cpu0.as_mut())
        .unwrap()
        .expect("an overloaded Deadline runqueue must push one reservation");
    assert_eq!(cpu0.deadline_bandwidth().this_bw_scaled(), 100_000_000);
    assert_eq!(cpu1.deadline_bandwidth().this_bw_scaled(), 0);
    system.drain_policy_updates_at(cpu1.as_mut(), 1).unwrap();
    assert_eq!(cpu1.deadline_bandwidth().this_bw_scaled(), 100_000_000);
    assert_eq!(
        system.deadline_activity(migrated).unwrap().bandwidth_cpu(),
        Some(CpuId::new(1))
    );
}

#[test]
fn queued_policy_change_replaces_the_deadline_reservation_accounting() {
    let (system, mut cpu) = online_system();
    let thread = ready_deadline(&system, 4, 8, 8, DeadlineFlags::NONE);
    system.enqueue_at(cpu.as_mut(), thread.id(), 0).unwrap();
    assert_eq!(cpu.deadline_bandwidth().this_bw_scaled(), 500_000_000);

    system
        .set_thread_policy(thread.id(), SchedulePolicy::default())
        .unwrap();
    system.drain_policy_updates_at(cpu.as_mut(), 1).unwrap();
    assert_eq!(cpu.deadline_bandwidth().this_bw_scaled(), 0);

    let replacement =
        SchedulePolicy::deadline(DeadlinePolicy::new(2, 10, 20, DeadlineFlags::NONE).unwrap());
    system.set_thread_policy(thread.id(), replacement).unwrap();
    system.drain_policy_updates_at(cpu.as_mut(), 2).unwrap();
    assert_eq!(cpu.deadline_bandwidth().this_bw_scaled(), 100_000_000);
    assert_eq!(
        system.deadline_activity(thread.id()).unwrap().activity(),
        DeadlineActivity::ActiveContending
    );
}

#[test]
fn scheduler_capacity_is_rejected_before_deadline_runqueue_publication() {
    support::clear_handles();
    let system = TaskSystem::new(TaskSystemConfig::new(1).with_thread_capacity(2)).unwrap();
    let mut cpu = system.create_cpu_local(CpuId::new(0)).unwrap();
    system
        .register_idle_thread(
            cpu.as_mut(),
            ThreadSpec::new(SchedulePolicy::fair(Nice::ZERO, FairMode::Idle)),
        )
        .unwrap();
    system.bring_cpu_online(cpu.as_mut()).unwrap();
    let first = ready_deadline(&system, 1, 10, 10, DeadlineFlags::NONE);
    system.enqueue_at(cpu.as_mut(), first.id(), 0).unwrap();
    let second_policy =
        SchedulePolicy::deadline(DeadlinePolicy::new(1, 10, 10, DeadlineFlags::NONE).unwrap());
    assert_eq!(
        system.create_thread(ThreadSpec::new(second_policy)),
        Err(TaskError::ThreadCapacity)
    );

    assert_eq!(cpu.deadline_bandwidth().this_bw_scaled(), 100_000_000);
}

#[test]
fn blocked_deadline_exit_waits_for_owner_member_cleanup() {
    let (system, mut cpu) = online_system();
    let thread = ready_deadline(&system, 1, 10, 10, DeadlineFlags::NONE);
    system.enqueue_at(cpu.as_mut(), thread.id(), 0).unwrap();
    assert_eq!(
        system.schedule_at(cpu.as_mut(), 0).unwrap().next(),
        thread.id()
    );
    system.block_current_at(cpu.as_mut(), 0).unwrap();
    system.complete_context_switch(cpu.as_mut()).unwrap();

    assert_eq!(system.mark_exited(thread.id()), Err(TaskError::ThreadBusy));
    assert_eq!(cpu.deadline_bandwidth().this_bw_scaled(), 100_000_000);
    let drain = system.drain_policy_updates_at(cpu.as_mut(), 0).unwrap();
    assert_eq!((drain.drained(), drain.pending()), (1, false));
    assert_eq!(
        system
            .deadline_activity(thread.id())
            .unwrap()
            .bandwidth_cpu(),
        None
    );
    system.mark_exited(thread.id()).unwrap();

    assert_eq!(cpu.deadline_bandwidth().this_bw_scaled(), 0);
    assert_eq!(thread.state(), ThreadState::Exited);
}

fn online_system() -> (TaskSystem, core::pin::Pin<Box<ax_task::CpuLocal>>) {
    support::clear_handles();
    let system = TaskSystem::new(TaskSystemConfig::new(1)).unwrap();
    let mut cpu = system.create_cpu_local(CpuId::new(0)).unwrap();
    system
        .register_idle_thread(
            cpu.as_mut(),
            ThreadSpec::new(SchedulePolicy::fair(Nice::ZERO, FairMode::Idle)),
        )
        .unwrap();
    system.bring_cpu_online(cpu.as_mut()).unwrap();
    (system, cpu)
}

fn ready_deadline(
    system: &TaskSystem,
    runtime_ns: u64,
    deadline_ns: u64,
    period_ns: u64,
    flags: DeadlineFlags,
) -> ax_task::ThreadHandle {
    let policy = SchedulePolicy::deadline(
        DeadlinePolicy::new(runtime_ns, deadline_ns, period_ns, flags).unwrap(),
    );
    let thread = system.create_thread(ThreadSpec::new(policy)).unwrap();
    system.make_ready(thread.id()).unwrap();
    thread
}

fn install_runtime_handles(system: &TaskSystem, cpu: core::pin::Pin<&mut ax_task::CpuLocal>) {
    support::install_handles((system as *const TaskSystem).expose_provenance(), cpu);
}
