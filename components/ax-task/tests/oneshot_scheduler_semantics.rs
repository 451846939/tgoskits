use ax_task::{
    CpuId, DEFAULT_RR_QUANTUM_NS, DeadlineFlags, DeadlinePolicy, FairMode,
    NORMALIZED_FAIR_SLICE_NS, Nice, RtPriority, SchedulePolicy, TaskSystem, TaskSystemConfig,
    ThreadSpec,
};

pub mod support;
use support::TaskSystemClockTestExt;

#[test]
fn sole_fair_dispatch_does_not_program_a_service_request() {
    let (system, mut cpu) = online_system();
    let fair = ready_thread(&system, SchedulePolicy::default());
    system.enqueue_at(cpu.as_mut(), fair.id(), 100).unwrap();

    assert_eq!(
        system.schedule_at(cpu.as_mut(), 100).unwrap().next(),
        fair.id()
    );
    assert_eq!(support::last_oneshot_ns(), 0);
    let (generation, deadline_ns) = support::last_scheduler_deadline_update();
    assert_ne!(generation, 0);
    assert_eq!(deadline_ns, 0);
}

#[test]
fn contended_fair_dispatch_programs_its_remaining_service_request() {
    let (system, mut cpu) = online_system();
    let first = ready_thread(&system, SchedulePolicy::default());
    let second = ready_thread(&system, SchedulePolicy::default());
    system.enqueue_at(cpu.as_mut(), first.id(), 100).unwrap();
    system.enqueue_at(cpu.as_mut(), second.id(), 100).unwrap();
    support::set_monotonic_ns(100);

    let selected = system.schedule_at(cpu.as_mut(), 100).unwrap().next();
    assert!(selected == first.id() || selected == second.id());
    let initial_deadline = 100 + NORMALIZED_FAIR_SLICE_NS / 2;
    assert_eq!(support::last_oneshot_ns(), initial_deadline);
    let (generation, deadline_ns) = support::last_scheduler_deadline_update();
    assert_ne!(generation, 0);
    assert_eq!(deadline_ns, initial_deadline);
}

#[test]
fn round_robin_dispatch_programs_its_remaining_quantum() {
    let (system, mut cpu) = online_system();
    let rr = ready_thread(
        &system,
        SchedulePolicy::round_robin(RtPriority::new(40).unwrap()),
    );
    system.enqueue_at(cpu.as_mut(), rr.id(), 100).unwrap();
    support::set_monotonic_ns(100);

    assert_eq!(
        system.schedule_at(cpu.as_mut(), 100).unwrap().next(),
        rr.id()
    );
    assert_eq!(support::last_oneshot_ns(), 100 + DEFAULT_RR_QUANTUM_NS);
}

#[test]
fn deadline_dispatch_programs_budget_before_its_absolute_deadline() {
    let (system, mut cpu) = online_system();
    let deadline = ready_thread(
        &system,
        SchedulePolicy::deadline(DeadlinePolicy::new(2, 10, 100, DeadlineFlags::NONE).unwrap()),
    );
    support::set_monotonic_ns(100);
    system.enqueue_at(cpu.as_mut(), deadline.id(), 100).unwrap();

    assert_eq!(
        system.schedule_at(cpu.as_mut(), 100).unwrap().next(),
        deadline.id()
    );
    assert_eq!(support::last_oneshot_ns(), 102);
}

#[test]
fn scheduler_boundary_is_translated_by_delta_into_the_monotonic_clock_domain() {
    let (system, mut cpu) = online_system();
    let deadline = ready_thread(
        &system,
        SchedulePolicy::deadline(DeadlinePolicy::new(2, 10, 100, DeadlineFlags::NONE).unwrap()),
    );
    support::set_monotonic_ns(10);
    system.enqueue_at(cpu.as_mut(), deadline.id(), 100).unwrap();

    assert_eq!(
        system.schedule_at(cpu.as_mut(), 100).unwrap().next(),
        deadline.id()
    );
    assert_eq!(
        support::last_oneshot_ns(),
        12,
        "a scheduler deadline must move by its 2ns forward delta, not copy its 102ns rq epoch"
    );
}

#[test]
fn fifo_dispatch_programs_the_rt_quota_exhaustion_boundary() {
    let (system, mut cpu) = online_system();
    let fifo = ready_thread(&system, SchedulePolicy::fifo(RtPriority::new(40).unwrap()));
    system.enqueue_at(cpu.as_mut(), fifo.id(), 100).unwrap();
    support::set_monotonic_ns(100);

    assert_eq!(
        system.schedule_at(cpu.as_mut(), 100).unwrap().next(),
        fifo.id()
    );
    assert_eq!(support::last_oneshot_ns(), 950_000_101);
}

#[test]
fn rt_period_expiry_reschedules_throttled_fifo_from_dedicated_idle() {
    let (system, mut cpu) = online_system();
    let fifo = ready_thread(&system, SchedulePolicy::fifo(RtPriority::new(40).unwrap()));
    system.enqueue_at(cpu.as_mut(), fifo.id(), 0).unwrap();
    support::set_monotonic_ns(0);

    assert_eq!(
        system.schedule_at(cpu.as_mut(), 0).unwrap().next(),
        fifo.id()
    );
    support::install_handles(
        (&system as *const TaskSystem).expose_provenance(),
        cpu.as_mut(),
    );

    support::set_monotonic_ns(950_000_001);
    support::set_scheduler_ns(950_000_001);
    let exhausted = ax_task::on_clock_event(
        ax_task::runtime::MonotonicInstant::from_nanos(950_000_001).unwrap(),
        64,
    )
    .unwrap();
    assert_eq!(
        exhausted
            .next_deadline()
            .map(|deadline| deadline.as_nanos()),
        Some(1_000_000_000),
        "RT exhaustion must arm the bandwidth period timer",
    );
    assert_ne!(
        system
            .schedule_if_requested_at(cpu.as_mut(), 950_000_001)
            .unwrap()
            .decision()
            .expect("quota exhaustion must switch away from FIFO")
            .next(),
        fifo.id(),
    );

    support::set_monotonic_ns(1_000_000_000);
    support::set_scheduler_ns(1_000_000_000);
    let replenished = ax_task::on_clock_event(
        ax_task::runtime::MonotonicInstant::from_nanos(1_000_000_000).unwrap(),
        64,
    )
    .unwrap();
    assert!(
        replenished.pending(),
        "the RT period owner must publish reschedule work when it unthrottles a runnable FIFO task",
    );
    assert_eq!(
        system
            .schedule_if_requested_at(cpu.as_mut(), 1_000_000_000)
            .unwrap()
            .decision()
            .expect("period replenishment must leave dedicated idle")
            .next(),
        fifo.id(),
    );
    support::clear_handles();
}

#[test]
fn blocking_fifo_reprograms_the_fair_successor_deadline() {
    let (system, mut cpu) = online_system();
    let fifo = ready_thread(&system, SchedulePolicy::fifo(RtPriority::new(40).unwrap()));
    let fair = ready_thread(&system, SchedulePolicy::default());
    let fair_contender = ready_thread(&system, SchedulePolicy::default());
    system.enqueue_at(cpu.as_mut(), fifo.id(), 100).unwrap();
    system.enqueue_at(cpu.as_mut(), fair.id(), 100).unwrap();
    system
        .enqueue_at(cpu.as_mut(), fair_contender.id(), 100)
        .unwrap();
    support::set_monotonic_ns(100);

    assert_eq!(
        system.schedule_at(cpu.as_mut(), 100).unwrap().next(),
        fifo.id()
    );
    assert_eq!(support::last_oneshot_ns(), 10_000_000);

    support::set_monotonic_ns(200);
    assert_eq!(
        system.block_current_at(cpu.as_mut(), 200).unwrap().next(),
        fair.id()
    );
    assert_eq!(
        support::last_oneshot_ns(),
        200 + NORMALIZED_FAIR_SLICE_NS / 2,
        "a forced block must replace the outgoing RT deadline with the selected Fair request",
    );
}

#[test]
fn exiting_fifo_reprograms_the_fair_successor_deadline() {
    support::clear_handles();
    let system = TaskSystem::new(TaskSystemConfig::new(1)).unwrap();
    let mut cpu = system.create_cpu_local(CpuId::new(0)).unwrap();
    let fifo = system
        .install_bootstrap_thread(
            cpu.as_mut(),
            ThreadSpec::new(SchedulePolicy::fifo(RtPriority::new(40).unwrap())),
        )
        .unwrap();
    system
        .register_idle_thread(
            cpu.as_mut(),
            ThreadSpec::new(SchedulePolicy::fair(Nice::ZERO, FairMode::Idle)),
        )
        .unwrap();
    system.bring_cpu_online(cpu.as_mut()).unwrap();
    let fair = ready_thread(&system, SchedulePolicy::default());
    let fair_contender = ready_thread(&system, SchedulePolicy::default());
    system.enqueue_at(cpu.as_mut(), fair.id(), 100).unwrap();
    system
        .enqueue_at(cpu.as_mut(), fair_contender.id(), 100)
        .unwrap();
    support::set_monotonic_ns(100);

    assert_eq!(
        system.schedule_at(cpu.as_mut(), 100).unwrap().next(),
        fifo.id()
    );
    assert_eq!(support::last_oneshot_ns(), 10_000_000);

    support::set_monotonic_ns(200);
    assert_eq!(
        system.exit_current_at(cpu.as_mut(), 200).unwrap().next(),
        fair.id()
    );
    assert_eq!(
        support::last_oneshot_ns(),
        200 + NORMALIZED_FAIR_SLICE_NS / 2
    );
}

#[test]
fn constrained_deadline_replenishment_preemption_is_seen_in_the_same_safe_point() {
    let (system, mut cpu) = online_system();
    let deadline = ready_thread(
        &system,
        SchedulePolicy::deadline(DeadlinePolicy::new(1, 10, 100, DeadlineFlags::NONE).unwrap()),
    );
    let fair = ready_thread(&system, SchedulePolicy::default());
    system.enqueue_at(cpu.as_mut(), deadline.id(), 0).unwrap();
    system.enqueue_at(cpu.as_mut(), fair.id(), 0).unwrap();
    assert_eq!(
        system.schedule_at(cpu.as_mut(), 0).unwrap().next(),
        deadline.id()
    );
    assert!(
        system
            .charge_current_at(cpu.as_mut(), 1, 1, 0)
            .unwrap()
            .slice_expired()
    );
    support::set_monotonic_ns(1);
    assert_eq!(
        system.schedule_at(cpu.as_mut(), 1).unwrap().next(),
        fair.id()
    );
    support::set_monotonic_ns(2);
    let _consumed_prior_request = system.schedule_if_requested_at(cpu.as_mut(), 2).unwrap();
    assert!(
        system
            .schedule_if_requested_at(cpu.as_mut(), 2)
            .unwrap()
            .decision()
            .is_none()
    );
    assert_eq!(
        support::last_oneshot_ns(),
        100,
        "budget depletion must arm the next release rather than the earlier relative deadline",
    );

    support::install_handles(
        (&system as *const TaskSystem).expose_provenance(),
        cpu.as_mut(),
    );
    support::set_monotonic_ns(100);
    support::set_scheduler_ns(100);
    let event = ax_task::on_clock_event(
        ax_task::runtime::MonotonicInstant::from_nanos(100).unwrap(),
        64,
    )
    .unwrap();
    assert_eq!(event.expired(), 1);
    ax_task::runtime::task_runtime::publish_scheduler_deadline(event.update());
    let decision = system
        .schedule_if_requested_at(cpu.as_mut(), 100)
        .unwrap()
        .decision()
        .expect("replenishment must be reconsidered before leaving this safe point");
    assert_eq!(decision.next(), deadline.id());
    support::clear_handles();
}

#[test]
fn yielded_deadline_rearms_replenishment_after_earlier_zero_lag_event() {
    let (system, mut cpu) = online_system();
    let deadline = ready_thread(
        &system,
        SchedulePolicy::deadline(DeadlinePolicy::new(2, 10, 100, DeadlineFlags::NONE).unwrap()),
    );
    system.enqueue_at(cpu.as_mut(), deadline.id(), 0).unwrap();
    assert_eq!(
        system.schedule_at(cpu.as_mut(), 0).unwrap().next(),
        deadline.id()
    );

    support::set_monotonic_ns(1);
    system.yield_current_at(cpu.as_mut(), 1).unwrap();
    assert_eq!(support::last_oneshot_ns(), 10, "zero-lag must fire first");

    support::install_handles(
        (&system as *const TaskSystem).expose_provenance(),
        cpu.as_mut(),
    );
    support::set_monotonic_ns(10);
    support::set_scheduler_ns(10);
    let event = ax_task::on_clock_event(
        ax_task::runtime::MonotonicInstant::from_nanos(10).unwrap(),
        64,
    )
    .unwrap();
    assert_eq!(
        event.expired(),
        1,
        "zero-lag must arrive as one typed task deadline"
    );
    assert!(
        !event.pending(),
        "no second immediately due event remains after the bounded IRQ pass"
    );
    ax_task::runtime::task_runtime::publish_scheduler_deadline(event.update());
    system.schedule_at(cpu.as_mut(), 10).unwrap();
    assert_eq!(
        support::last_oneshot_ns(),
        100,
        "zero-lag servicing must preserve the later CBS replenishment",
    );
    support::set_monotonic_ns(100);
    support::set_scheduler_ns(100);
    let event = ax_task::on_clock_event(
        ax_task::runtime::MonotonicInstant::from_nanos(100).unwrap(),
        64,
    )
    .unwrap();
    assert_eq!(event.expired(), 1);
    ax_task::runtime::task_runtime::publish_scheduler_deadline(event.update());
    assert_eq!(
        system
            .schedule_if_requested_at(cpu.as_mut(), 100)
            .unwrap()
            .decision()
            .expect("CBS replenishment must preempt idle")
            .next(),
        deadline.id()
    );
    support::clear_handles();
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

fn ready_thread(system: &TaskSystem, policy: SchedulePolicy) -> ax_task::ThreadHandle {
    let thread = system.create_thread(ThreadSpec::new(policy)).unwrap();
    system.make_ready(thread.id()).unwrap();
    thread
}
