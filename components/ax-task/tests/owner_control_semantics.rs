use ax_task::{
    CpuId, DeadlineFlags, DeadlinePolicy, FairMode, Nice, RtPriority, SchedulePolicy, TaskError,
    TaskSystem, TaskSystemConfig, ThreadSpec,
};

pub mod support;
use support::TaskSystemClockTestExt;

#[test]
fn queued_policy_update_commits_in_one_owner_rq_transaction() {
    let (system, mut cpu) = online_system(1);
    let fair = ready_thread(&system, SchedulePolicy::default());
    let promoted = ready_thread(&system, SchedulePolicy::default());
    system.enqueue_at(cpu.as_mut(), fair.id(), 0).unwrap();
    system.enqueue_at(cpu.as_mut(), promoted.id(), 0).unwrap();

    let fifo = SchedulePolicy::fifo(RtPriority::new(80).unwrap());
    system.set_thread_policy(promoted.id(), fifo).unwrap();
    assert_eq!(promoted.policy(), fifo);
    assert_eq!(promoted.effective_policy(), fifo);
    assert_eq!(
        system
            .drain_owner_control_at(cpu.as_mut(), 1)
            .unwrap()
            .drained(),
        0,
        "policy changes must not leave a second-stage owner delivery"
    );
    assert_eq!(
        system.schedule_at(cpu.as_mut(), 1).unwrap().next(),
        promoted.id()
    );
}

#[test]
fn running_policy_update_survives_old_dispatch_commit() {
    let (system, mut cpu) = online_system(1);
    let running = ready_thread(&system, SchedulePolicy::default());
    system.enqueue_at(cpu.as_mut(), running.id(), 0).unwrap();
    assert_eq!(
        system.schedule_at(cpu.as_mut(), 0).unwrap().next(),
        running.id()
    );
    system.charge_current_at(cpu.as_mut(), 7, 7, 0).unwrap();

    let fifo = SchedulePolicy::fifo(RtPriority::new(80).unwrap());
    system.set_thread_policy(running.id(), fifo).unwrap();
    assert_eq!(
        system
            .drain_owner_control_at(cpu.as_mut(), 7)
            .unwrap()
            .drained(),
        0
    );
    assert_eq!(running.effective_policy(), fifo);

    let lower = ready_thread(&system, SchedulePolicy::fifo(RtPriority::new(70).unwrap()));
    system.enqueue_at(cpu.as_mut(), lower.id(), 7).unwrap();
    assert_eq!(
        system.yield_current_at(cpu.as_mut(), 8).unwrap().next(),
        running.id()
    );
}

#[test]
fn remote_running_policy_update_commits_before_returning() {
    support::clear_handles();
    let system = TaskSystem::new(TaskSystemConfig::new(2)).unwrap();
    let mut cpu0 = system.create_cpu_local(CpuId::new(0)).unwrap();
    let mut cpu1 = system.create_cpu_local(CpuId::new(1)).unwrap();
    system.bring_cpu_online(cpu0.as_mut()).unwrap();
    system.bring_cpu_online(cpu1.as_mut()).unwrap();
    support::install_handles(
        (&system as *const TaskSystem).expose_provenance(),
        cpu0.as_mut(),
    );
    support::install_cpu(1, cpu1.as_mut());
    support::set_online_cpu_count(2);

    let running = ready_thread(&system, SchedulePolicy::default());
    system.enqueue_at(cpu1.as_mut(), running.id(), 0).unwrap();
    assert_eq!(
        system.schedule_at(cpu1.as_mut(), 0).unwrap().next(),
        running.id()
    );

    let fifo = SchedulePolicy::fifo(RtPriority::new(60).unwrap());
    system.set_thread_policy(running.id(), fifo).unwrap();
    assert_eq!(running.policy(), fifo);
    assert_eq!(running.effective_policy(), fifo);
    assert_eq!(
        system
            .drain_owner_control_at(cpu1.as_mut(), 1)
            .unwrap()
            .drained(),
        0,
        "a remote rq transaction may request rescheduling but must not defer policy ownership"
    );
    support::clear_handles();
}

#[test]
fn owner_applies_deadline_to_fair_and_fair_to_deadline_transitions() {
    let (system, mut cpu) = online_system(1);
    let thread = ready_thread(&system, SchedulePolicy::default());
    system.enqueue_at(cpu.as_mut(), thread.id(), 0).unwrap();

    let deadline =
        SchedulePolicy::deadline(DeadlinePolicy::new(2, 5, 10, DeadlineFlags::NONE).unwrap());
    system.set_thread_policy(thread.id(), deadline).unwrap();
    assert_eq!(
        system.schedule_at(cpu.as_mut(), 3).unwrap().next(),
        thread.id()
    );
    assert_eq!(thread.effective_policy(), deadline);
    assert_eq!(
        system
            .deadline_runtime(thread.id())
            .unwrap()
            .remaining_runtime_ns(),
        2
    );

    let fair = SchedulePolicy::fair(Nice::new(5).unwrap(), FairMode::Normal);
    system.set_thread_policy(thread.id(), fair).unwrap();
    assert_eq!(thread.effective_policy(), fair);
    assert_eq!(
        system.deadline_runtime(thread.id()),
        Err(TaskError::InvalidConfiguration)
    );
}

#[test]
fn back_to_back_policy_updates_publish_each_committed_generation() {
    let (system, mut cpu) = online_system(1);
    let thread = ready_thread(&system, SchedulePolicy::default());
    system.enqueue_at(cpu.as_mut(), thread.id(), 0).unwrap();

    let stale = SchedulePolicy::fifo(RtPriority::new(90).unwrap());
    let latest = SchedulePolicy::fair(Nice::new(10).unwrap(), FairMode::Batch);
    system.set_thread_policy(thread.id(), stale).unwrap();
    assert_eq!(thread.effective_policy(), stale);
    system.set_thread_policy(thread.id(), latest).unwrap();
    assert_eq!(thread.policy(), latest);
    assert_eq!(thread.effective_policy(), latest);

    assert_eq!(
        system
            .drain_owner_control_at(cpu.as_mut(), 1)
            .unwrap()
            .drained(),
        0
    );
}

#[test]
fn committed_policy_update_does_not_pin_exited_thread_resources() {
    let (system, mut cpu) = online_system(1);
    let thread = ready_thread(&system, SchedulePolicy::default());
    let thread_id = thread.id();
    system.enqueue_at(cpu.as_mut(), thread_id, 0).unwrap();

    system
        .set_thread_policy(
            thread_id,
            SchedulePolicy::fifo(RtPriority::new(80).unwrap()),
        )
        .unwrap();
    system.dequeue(cpu.as_mut(), thread_id).unwrap();
    system.mark_exited(thread_id).unwrap();
    drop(thread);

    assert_eq!(
        system
            .service_deferred_task_work(ax_task::DEFAULT_BATCH_LIMIT)
            .unwrap()
            .processed(),
        1,
        "a committed rq transaction must not leave an inbox-owned thread reference"
    );
    assert_eq!(
        system.thread_state(thread_id),
        Err(TaskError::StaleThreadId)
    );
}

#[test]
fn exited_thread_rejects_policy_and_affinity_mutation() {
    let (system, _cpu) = online_system(1);
    let thread = system
        .create_thread(ThreadSpec::new(SchedulePolicy::default()))
        .unwrap();
    system.mark_exited(thread.id()).unwrap();

    assert_eq!(
        system.set_thread_policy(
            thread.id(),
            SchedulePolicy::fifo(RtPriority::new(80).unwrap()),
        ),
        Err(TaskError::NotReady)
    );
    assert_eq!(
        system.set_affinity(thread.id(), ax_task::CpuSet::all(1)),
        Err(TaskError::NotReady)
    );
}

#[test]
fn deadline_to_fair_releases_admission_before_returning() {
    let (system, mut cpu) = online_system(1);
    let active = ready_thread(&system, deadline(90, 100));
    system.enqueue_at(cpu.as_mut(), active.id(), 0).unwrap();

    system
        .set_thread_policy(active.id(), SchedulePolicy::default())
        .unwrap();
    system
        .create_thread(ThreadSpec::new(deadline(10, 100)))
        .unwrap();
    assert_eq!(
        system
            .drain_owner_control_at(cpu.as_mut(), 1)
            .unwrap()
            .drained(),
        0
    );
}

#[test]
fn deadline_reduction_releases_admission_before_returning() {
    let (system, mut cpu) = online_system(1);
    let active = ready_thread(&system, deadline(90, 100));
    system.enqueue_at(cpu.as_mut(), active.id(), 0).unwrap();

    system
        .set_thread_policy(active.id(), deadline(50, 100))
        .unwrap();
    system
        .create_thread(ThreadSpec::new(deadline(45, 100)))
        .unwrap();
    assert_eq!(
        system
            .drain_owner_control_at(cpu.as_mut(), 1)
            .unwrap()
            .drained(),
        0
    );
}

#[test]
fn fair_to_deadline_reserves_admission_before_returning() {
    let (system, mut cpu) = online_system(1);
    let active = ready_thread(&system, SchedulePolicy::default());
    system.enqueue_at(cpu.as_mut(), active.id(), 0).unwrap();

    system
        .set_thread_policy(active.id(), deadline(90, 100))
        .unwrap();
    assert!(matches!(
        system.create_thread(ThreadSpec::new(deadline(10, 100))),
        Err(TaskError::DeadlineAdmission)
    ));

    assert_eq!(
        system
            .drain_owner_control_at(cpu.as_mut(), 1)
            .unwrap()
            .drained(),
        0,
        "deadline admission and policy publication are one owner-rq transaction"
    );
}

fn online_system(cpu_count: usize) -> (TaskSystem, core::pin::Pin<Box<ax_task::CpuLocal>>) {
    support::clear_handles();
    let system = TaskSystem::new(TaskSystemConfig::new(cpu_count)).unwrap();
    let mut cpu = system.create_cpu_local(CpuId::new(0)).unwrap();
    system.bring_cpu_online(cpu.as_mut()).unwrap();
    (system, cpu)
}

fn ready_thread(system: &TaskSystem, policy: SchedulePolicy) -> ax_task::ThreadHandle {
    let thread = system.create_thread(ThreadSpec::new(policy)).unwrap();
    system.make_ready(thread.id()).unwrap();
    thread
}

fn deadline(runtime_ns: u64, period_ns: u64) -> SchedulePolicy {
    SchedulePolicy::deadline(
        DeadlinePolicy::new(runtime_ns, period_ns, period_ns, DeadlineFlags::NONE).unwrap(),
    )
}
