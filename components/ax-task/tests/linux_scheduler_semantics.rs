// SPDX-License-Identifier: Apache-2.0
//! Original tests for Linux-compatible scheduler behavior.
//!
//! These tests rewrite observable semantics described by Linux documentation;
//! they do not copy GPL-licensed kernel or selftest source code.
//! Semantic sources:
//! - <https://docs.kernel.org/scheduler/sched-rt-group.html>
//! - <https://docs.kernel.org/scheduler/sched-deadline.html>
//! - <https://docs.kernel.org/trace/rv/monitor_sched.html>

use core::sync::atomic::{AtomicUsize, Ordering};

use ax_task::{
    CpuId, CpuSet, DeadlineFlags, DeadlinePolicy, FairMode, Nice, PiMutexCore, RtPriority,
    SchedulePolicy, TaskError, TaskSystem, TaskSystemConfig, ThreadExtension, ThreadExtensionOps,
    ThreadId, ThreadSpec, ThreadState,
};

pub mod support;
use support::TaskSystemClockTestExt;

#[test]
fn enqueue_preemption_remains_sticky_until_scheduler_entry() {
    support::clear_handles();
    let (system, mut cpu) = online_system(TaskSystemConfig::new(1));
    let thread = ready_thread(&system, SchedulePolicy::default());
    system.enqueue_at(cpu.as_mut(), thread.id(), 0).unwrap();

    assert!(system.snapshot(cpu.as_ref()).unwrap().need_resched());
    assert_eq!(
        system.schedule_at(cpu.as_mut(), 0).unwrap().next(),
        thread.id()
    );
    assert!(!system.snapshot(cpu.as_ref()).unwrap().need_resched());
}

#[test]
fn rt_bandwidth_throttles_at_quota_until_the_next_period() {
    let (system, mut cpu) = online_system(TaskSystemConfig::new(1));
    let thread = ready_thread(&system, SchedulePolicy::fifo(RtPriority::new(80).unwrap()));
    let fair = ready_thread(&system, SchedulePolicy::default());
    system.enqueue_at(cpu.as_mut(), thread.id(), 0).unwrap();
    system.enqueue_at(cpu.as_mut(), fair.id(), 0).unwrap();
    system.schedule_at(cpu.as_mut(), 0).unwrap();

    system
        .charge_current_at(cpu.as_mut(), 0, 950_000_001, 0)
        .unwrap();

    assert!(!system.rt_run_queue_may_run_at(cpu.as_mut(), 0).unwrap());
    assert_eq!(
        system.schedule_at(cpu.as_mut(), 0).unwrap().next(),
        fair.id()
    );
    support::install_handles(
        (&system as *const TaskSystem).expose_provenance(),
        cpu.as_mut(),
    );
    support::set_monotonic_ns(1_000_000_000);
    support::set_scheduler_ns(1_000_000_000);
    ax_task::on_clock_event(
        ax_task::runtime::MonotonicInstant::from_nanos(1_000_000_000).unwrap(),
        64,
    )
    .unwrap();
    assert!(
        system
            .rt_run_queue_may_run_at(cpu.as_mut(), 1_000_000_000)
            .unwrap()
    );
    support::clear_handles();
}

#[test]
fn exhausted_rt_bandwidth_skips_ordinary_rt_until_the_next_period() {
    let (system, mut cpu) = online_system(TaskSystemConfig::new(1));
    let rt = ready_thread(&system, SchedulePolicy::fifo(RtPriority::new(80).unwrap()));
    let fair = ready_thread(&system, SchedulePolicy::default());
    system.enqueue_at(cpu.as_mut(), rt.id(), 0).unwrap();
    system.enqueue_at(cpu.as_mut(), fair.id(), 0).unwrap();
    assert_eq!(system.schedule_at(cpu.as_mut(), 0).unwrap().next(), rt.id());

    system
        .charge_current_at(cpu.as_mut(), 950_000_001, 950_000_001, 0)
        .unwrap();
    assert_eq!(
        system
            .schedule_at(cpu.as_mut(), 950_000_001)
            .unwrap()
            .next(),
        fair.id()
    );
    support::install_handles(
        (&system as *const TaskSystem).expose_provenance(),
        cpu.as_mut(),
    );
    support::set_monotonic_ns(1_000_000_000);
    support::set_scheduler_ns(1_000_000_000);
    ax_task::on_clock_event(
        ax_task::runtime::MonotonicInstant::from_nanos(1_000_000_000).unwrap(),
        64,
    )
    .unwrap();
    assert_eq!(
        system
            .schedule_at(cpu.as_mut(), 1_000_000_000)
            .unwrap()
            .next(),
        rt.id()
    );
    support::clear_handles();
}

#[test]
fn pi_boosted_rt_owner_runs_past_quota_to_release_the_lock() {
    let (system, mut cpu) = online_system(TaskSystemConfig::new(1));
    let owner = ready_thread(&system, SchedulePolicy::default());
    let competitor = ready_thread(&system, SchedulePolicy::default());
    let waiter = ready_thread(&system, SchedulePolicy::fifo(RtPriority::new(90).unwrap()));
    system.enqueue_at(cpu.as_mut(), owner.id(), 0).unwrap();
    assert_eq!(
        system.schedule_at(cpu.as_mut(), 0).unwrap().next(),
        owner.id()
    );

    let lock = PiMutexCore::new();
    let wait = support::commit_pi_wait(&system, &lock, waiter.id(), owner.id()).unwrap();
    system.drain_owner_control_at(cpu.as_mut(), 0).unwrap();
    system.enqueue_at(cpu.as_mut(), competitor.id(), 0).unwrap();
    system
        .charge_current_at(cpu.as_mut(), 950_000_000, 950_000_000, 0)
        .unwrap();

    assert_eq!(
        system
            .schedule_at(cpu.as_mut(), 950_000_000)
            .unwrap()
            .next(),
        owner.id()
    );
    system.pi_wait_cancel(wait).unwrap();
}

#[test]
fn deadline_admission_enforces_the_root_domain_cap() {
    let (system, _cpu) = online_system(TaskSystemConfig::new(1));
    let half = deadline_policy(50, 100, 100, DeadlineFlags::NONE);
    let over_cap = deadline_policy(46, 100, 100, DeadlineFlags::NONE);

    system
        .create_thread(ThreadSpec::new(SchedulePolicy::deadline(half)))
        .unwrap();

    assert!(matches!(
        system.create_thread(ThreadSpec::new(SchedulePolicy::deadline(over_cap))),
        Err(TaskError::DeadlineAdmission)
    ));
}

#[test]
fn exited_deadline_releases_admission_before_late_handles_are_reaped() {
    let (system, cpu) = online_system(TaskSystemConfig::new(1));
    let policy = SchedulePolicy::deadline(deadline_policy(95, 100, 100, DeadlineFlags::NONE));
    let first = system.create_thread(ThreadSpec::new(policy)).unwrap();
    let first_id = first.id();

    system.mark_exited(first_id).unwrap();
    let second = system
        .create_thread(ThreadSpec::new(policy))
        .expect("Exited must release admission even while a strong handle remains");

    drop(first);
    system.reap_thread(first_id).unwrap();
    assert_eq!(
        system.create_thread(ThreadSpec::new(policy)).unwrap_err(),
        TaskError::DeadlineAdmission,
        "reaping a zeroed reservation must not release the live reservation twice",
    );

    system.mark_exited(second.id()).unwrap();
    drop(cpu);
}

#[test]
fn deadline_affinity_must_cover_the_online_root_domain() {
    let system = TaskSystem::new(TaskSystemConfig::new(2)).unwrap();
    let mut cpu0 = system.create_cpu_local(CpuId::new(0)).unwrap();
    let mut cpu1 = system.create_cpu_local(CpuId::new(1)).unwrap();
    system.bring_cpu_online(cpu0.as_mut()).unwrap();
    system.bring_cpu_online(cpu1.as_mut()).unwrap();
    let mut affinity = CpuSet::empty(2);
    affinity.insert(CpuId::new(0));
    let policy = deadline_policy(1, 10, 10, DeadlineFlags::NONE);

    assert!(matches!(
        system.create_thread(
            ThreadSpec::new(SchedulePolicy::deadline(policy)).with_affinity(affinity)
        ),
        Err(TaskError::DeadlineAffinity)
    ));
}

#[test]
fn edf_selects_the_earliest_absolute_deadline() {
    let (system, mut cpu) = online_system(TaskSystemConfig::new(1));
    let later = ready_thread(
        &system,
        SchedulePolicy::deadline(deadline_policy(1, 8, 20, DeadlineFlags::NONE)),
    );
    let earlier = ready_thread(
        &system,
        SchedulePolicy::deadline(deadline_policy(1, 5, 20, DeadlineFlags::NONE)),
    );
    system.enqueue_at(cpu.as_mut(), later.id(), 100).unwrap();
    system.enqueue_at(cpu.as_mut(), earlier.id(), 100).unwrap();

    assert_eq!(
        system.schedule_at(cpu.as_mut(), 100).unwrap().next(),
        earlier.id()
    );
}

#[test]
fn throttled_deadline_job_is_replenished_and_becomes_runnable() {
    let system = TaskSystem::new(TaskSystemConfig::new(1)).unwrap();
    let mut cpu = system.create_cpu_local(CpuId::new(0)).unwrap();
    system
        .register_idle_thread(
            cpu.as_mut(),
            ThreadSpec::new(SchedulePolicy::fair(Nice::ZERO, FairMode::Idle)),
        )
        .unwrap();
    system.bring_cpu_online(cpu.as_mut()).unwrap();
    let deadline = ready_thread(
        &system,
        SchedulePolicy::deadline(deadline_policy(5, 10, 20, DeadlineFlags::NONE)),
    );
    system.enqueue_at(cpu.as_mut(), deadline.id(), 0).unwrap();
    assert_eq!(
        system.schedule_at(cpu.as_mut(), 0).unwrap().next(),
        deadline.id()
    );
    let charge = system.charge_current_at(cpu.as_mut(), 5, 5, 0).unwrap();
    assert!(charge.slice_expired());
    assert!(!charge.deadline_overrun());
    support::set_monotonic_ns(5);
    assert_ne!(
        system.schedule_at(cpu.as_mut(), 5).unwrap().next(),
        deadline.id()
    );
    system.complete_context_switch(cpu.as_mut()).unwrap();
    assert_eq!(
        system.deadline_runtime(deadline.id()).unwrap().overruns(),
        1
    );

    support::set_monotonic_ns(20);
    assert_ne!(expire_deadline_irq(&system, cpu.as_mut(), 20), 0);
    assert_eq!(
        system.schedule_at(cpu.as_mut(), 20).unwrap().next(),
        deadline.id()
    );
}

#[test]
fn early_deadline_replenishment_keeps_the_runnable_job_throttled() {
    let system = TaskSystem::new(TaskSystemConfig::new(1)).unwrap();
    let mut cpu = system.create_cpu_local(CpuId::new(0)).unwrap();
    let idle = system
        .register_idle_thread(
            cpu.as_mut(),
            ThreadSpec::new(SchedulePolicy::fair(Nice::ZERO, FairMode::Idle)),
        )
        .unwrap();
    system.bring_cpu_online(cpu.as_mut()).unwrap();
    let deadline = ready_thread(
        &system,
        SchedulePolicy::deadline(deadline_policy(2, 10, 20, DeadlineFlags::NONE)),
    );
    system.enqueue_at(cpu.as_mut(), deadline.id(), 0).unwrap();
    assert_eq!(
        system.schedule_at(cpu.as_mut(), 0).unwrap().next(),
        deadline.id()
    );
    assert!(
        system
            .charge_current_at(cpu.as_mut(), 2, 2, 0)
            .unwrap()
            .slice_expired()
    );
    assert_eq!(
        system.schedule_at(cpu.as_mut(), 2).unwrap().next(),
        idle.id()
    );
    system.complete_context_switch(cpu.as_mut()).unwrap();
    assert_eq!(deadline.state(), ThreadState::Ready);

    support::set_monotonic_ns(10);
    assert_eq!(
        system.schedule_at(cpu.as_mut(), 10).unwrap().next(),
        idle.id()
    );
    assert_eq!(deadline.state(), ThreadState::Ready);
    support::set_monotonic_ns(20);
    assert_ne!(expire_deadline_irq(&system, cpu.as_mut(), 20), 0);
    assert_eq!(
        system.schedule_at(cpu.as_mut(), 20).unwrap().next(),
        deadline.id()
    );
}

#[test]
fn constrained_deadline_wake_after_deadline_waits_for_next_release() {
    let system = TaskSystem::new(TaskSystemConfig::new(1)).unwrap();
    let mut cpu = system.create_cpu_local(CpuId::new(0)).unwrap();
    let idle = system
        .register_idle_thread(
            cpu.as_mut(),
            ThreadSpec::new(SchedulePolicy::fair(Nice::ZERO, FairMode::Idle)),
        )
        .unwrap();
    system.bring_cpu_online(cpu.as_mut()).unwrap();
    let deadline = ready_thread(
        &system,
        SchedulePolicy::deadline(deadline_policy(2, 5, 10, DeadlineFlags::NONE)),
    );
    system.enqueue_at(cpu.as_mut(), deadline.id(), 0).unwrap();
    system.dequeue(cpu.as_mut(), deadline.id()).unwrap();

    support::set_monotonic_ns(9);
    system.enqueue_at(cpu.as_mut(), deadline.id(), 9).unwrap();

    assert_eq!(deadline.state(), ThreadState::Ready);
    support::set_monotonic_ns(9);
    assert_eq!(
        system.schedule_at(cpu.as_mut(), 9).unwrap().next(),
        idle.id()
    );
    support::set_monotonic_ns(10);
    assert_ne!(expire_deadline_irq(&system, cpu.as_mut(), 10), 0);
    assert_eq!(
        system.schedule_at(cpu.as_mut(), 10).unwrap().next(),
        deadline.id()
    );
}

#[test]
fn deadline_policy_rejects_the_linux_wrap_comparison_msb() {
    assert!(
        DeadlinePolicy::new(1, 1, 1_u64 << 63, DeadlineFlags::NONE).is_err(),
        "SCHED_DEADLINE relative intervals must stay inside the signed comparison window"
    );
}

#[test]
fn deadline_yield_ends_the_current_job_until_replenishment() {
    let system = TaskSystem::new(TaskSystemConfig::new(1)).unwrap();
    let mut cpu = system.create_cpu_local(CpuId::new(0)).unwrap();
    let idle = system
        .register_idle_thread(
            cpu.as_mut(),
            ThreadSpec::new(SchedulePolicy::fair(Nice::ZERO, FairMode::Idle)),
        )
        .unwrap();
    system.bring_cpu_online(cpu.as_mut()).unwrap();
    let deadline = ready_thread(
        &system,
        SchedulePolicy::deadline(deadline_policy(5, 10, 20, DeadlineFlags::NONE)),
    );
    system.enqueue_at(cpu.as_mut(), deadline.id(), 0).unwrap();
    assert_eq!(
        system.schedule_at(cpu.as_mut(), 0).unwrap().next(),
        deadline.id()
    );

    assert_eq!(
        system.yield_current_at(cpu.as_mut(), 1).unwrap().next(),
        idle.id()
    );
    system.complete_context_switch(cpu.as_mut()).unwrap();
    assert_eq!(
        system
            .deadline_runtime(deadline.id())
            .unwrap()
            .remaining_runtime_ns(),
        0
    );
    support::set_monotonic_ns(20);
    assert_ne!(expire_deadline_irq(&system, cpu.as_mut(), 20), 0);
    assert_eq!(
        system.schedule_at(cpu.as_mut(), 20).unwrap().next(),
        deadline.id()
    );
}

#[test]
fn active_deadline_job_does_not_arm_a_separate_miss_timer() {
    let (system, mut cpu) = online_system(TaskSystemConfig::new(1));
    let _idle = system
        .register_idle_thread(
            cpu.as_mut(),
            ThreadSpec::new(SchedulePolicy::fair(Nice::ZERO, FairMode::Idle)),
        )
        .unwrap();
    let deadline = ready_thread(
        &system,
        SchedulePolicy::deadline(deadline_policy(5, 10, 100, DeadlineFlags::NONE)),
    );
    system.enqueue_at(cpu.as_mut(), deadline.id(), 0).unwrap();
    assert_eq!(
        system.schedule_at(cpu.as_mut(), 0).unwrap().next(),
        deadline.id()
    );
    support::set_monotonic_ns(10);
    assert_eq!(expire_deadline_irq(&system, cpu.as_mut(), 10), 0);
    assert_eq!(
        system
            .deadline_runtime(deadline.id())
            .unwrap()
            .remaining_runtime_ns(),
        0
    );
    assert!(system.snapshot(cpu.as_ref()).unwrap().need_resched());
}

#[test]
fn deadline_overrun_flag_defers_notification_to_task_context() {
    DEADLINE_OVERRUNS.store(0, Ordering::Relaxed);
    let system = TaskSystem::new(TaskSystemConfig::new(1)).unwrap();
    let mut cpu = system.create_cpu_local(CpuId::new(0)).unwrap();
    system
        .register_idle_thread(
            cpu.as_mut(),
            ThreadSpec::new(SchedulePolicy::fair(Nice::ZERO, FairMode::Idle)),
        )
        .unwrap();
    system.bring_cpu_online(cpu.as_mut()).unwrap();
    let extension = unsafe { ThreadExtension::new(0, &DEADLINE_EXTENSION_OPS) };
    let deadline = system
        .create_thread(
            ThreadSpec::new(SchedulePolicy::deadline(deadline_policy(
                5,
                10,
                20,
                DeadlineFlags::DL_OVERRUN,
            )))
            .with_extension(extension),
        )
        .unwrap();
    system.make_ready(deadline.id()).unwrap();
    system.enqueue_at(cpu.as_mut(), deadline.id(), 0).unwrap();
    system.schedule_at(cpu.as_mut(), 0).unwrap();
    system.charge_current_at(cpu.as_mut(), 5, 5, 0).unwrap();
    system.schedule_at(cpu.as_mut(), 5).unwrap();

    assert_eq!(DEADLINE_OVERRUNS.load(Ordering::Relaxed), 0);
    assert_eq!(system.dispatch_deadline_overruns(1), Ok(1));
    assert_eq!(DEADLINE_OVERRUNS.load(Ordering::Relaxed), 1);
    assert_eq!(system.dispatch_deadline_overruns(1), Ok(0));
}

#[test]
fn affinity_change_of_running_thread_requests_migration_safe_point() {
    let system = TaskSystem::new(TaskSystemConfig::new(2)).unwrap();
    let mut cpu0 = system.create_cpu_local(CpuId::new(0)).unwrap();
    let mut cpu1 = system.create_cpu_local(CpuId::new(1)).unwrap();
    system
        .register_idle_thread(
            cpu0.as_mut(),
            ThreadSpec::new(SchedulePolicy::fair(Nice::ZERO, FairMode::Idle)),
        )
        .unwrap();
    system
        .register_idle_thread(
            cpu1.as_mut(),
            ThreadSpec::new(SchedulePolicy::fair(Nice::ZERO, FairMode::Idle)),
        )
        .unwrap();
    system.bring_cpu_online(cpu0.as_mut()).unwrap();
    system.bring_cpu_online(cpu1.as_mut()).unwrap();
    let thread = ready_thread(&system, SchedulePolicy::default());
    system.enqueue_at(cpu0.as_mut(), thread.id(), 0).unwrap();
    system.schedule_at(cpu0.as_mut(), 0).unwrap();
    let mut affinity = CpuSet::empty(2);
    affinity.insert(CpuId::new(1));

    system.set_affinity(thread.id(), affinity).unwrap();

    assert!(system.snapshot(cpu0.as_ref()).unwrap().need_resched());
    assert_eq!(
        system
            .drain_owner_control_at(cpu0.as_mut(), 1)
            .unwrap()
            .drained(),
        1
    );
    assert_ne!(
        system.schedule_at(cpu0.as_mut(), 1).unwrap().next(),
        thread.id()
    );
    // The target CPU cannot observe a runnable context until architecture
    // switch tail proves the source CPU has left the migrated thread's stack.
    system.complete_context_switch(cpu0.as_mut()).unwrap();
    assert_eq!(
        system
            .drain_owner_control_at(cpu1.as_mut(), 1)
            .unwrap()
            .drained(),
        1
    );
    assert_eq!(
        system.schedule_at(cpu1.as_mut(), 1).unwrap().next(),
        thread.id()
    );
}

fn online_system(config: TaskSystemConfig) -> (TaskSystem, core::pin::Pin<Box<ax_task::CpuLocal>>) {
    let system = TaskSystem::new(config).unwrap();
    let mut cpu = system.create_cpu_local(CpuId::new(0)).unwrap();
    system.bring_cpu_online(cpu.as_mut()).unwrap();
    (system, cpu)
}

fn ready_thread(system: &TaskSystem, policy: SchedulePolicy) -> ax_task::ThreadHandle {
    let thread = system.create_thread(ThreadSpec::new(policy)).unwrap();
    system.make_ready(thread.id()).unwrap();
    thread
}

fn expire_deadline_irq(
    system: &TaskSystem,
    mut cpu: core::pin::Pin<&mut ax_task::CpuLocal>,
    now_ns: u64,
) -> usize {
    support::install_handles(
        (system as *const TaskSystem).expose_provenance(),
        cpu.as_mut(),
    );
    support::set_monotonic_ns(now_ns);
    support::set_scheduler_ns(now_ns);
    let expired = ax_task::on_clock_event(
        ax_task::runtime::MonotonicInstant::from_nanos(now_ns).unwrap(),
        ax_task::DEFAULT_BATCH_LIMIT,
    )
    .unwrap()
    .expired();
    support::clear_handles();
    expired
}

fn deadline_policy(
    runtime_ns: u64,
    deadline_ns: u64,
    period_ns: u64,
    flags: DeadlineFlags,
) -> DeadlinePolicy {
    DeadlinePolicy::new(runtime_ns, deadline_ns, period_ns, flags).unwrap()
}

static DEADLINE_OVERRUNS: AtomicUsize = AtomicUsize::new(0);

static DEADLINE_EXTENSION_OPS: ThreadExtensionOps = ThreadExtensionOps {
    on_switch_in: no_extension_switch_in,
    on_switch_out: no_extension_switch_out,
    on_exit: no_extension_hook,
    on_deadline_overrun: count_deadline_overrun,
    drop: no_extension_drop,
};

unsafe extern "Rust" fn no_extension_hook(_data: usize, _thread: ThreadId) {}

unsafe extern "Rust" fn no_extension_switch_in(
    _data: usize,
    _thread: ThreadId,
    _policy: SchedulePolicy,
) {
}

unsafe extern "Rust" fn no_extension_switch_out(
    _data: usize,
    _thread: ThreadId,
    _reason: ax_task::SwitchReason,
) {
}

unsafe extern "Rust" fn count_deadline_overrun(_data: usize, _thread: ThreadId) {
    DEADLINE_OVERRUNS.fetch_add(1, Ordering::Relaxed);
}

unsafe extern "Rust" fn no_extension_drop(_data: usize) {}
