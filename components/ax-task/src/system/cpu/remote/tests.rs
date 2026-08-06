#[cfg(test)]
mod scheduler_ipi_tests {
    use std::{sync::mpsc, thread, time::Duration};

    use super::*;
    use crate::{TaskSystem, ThreadSpec, runtime::MonotonicInstant};

    #[test]
    fn fair_balance_clockevent_uses_monotonic_time_not_runqueue_time() {
        const BALANCE_INTERVAL_NS: u64 = 1_000;

        let system = TaskSystem::new(
            TaskSystemConfig::new(1).with_balance_interval_ns(BALANCE_INTERVAL_NS),
        )
        .unwrap();
        let mut cpu = system.create_cpu_local(CpuId::new(0)).unwrap();
        system
            .install_bootstrap_thread(cpu.as_mut(), ThreadSpec::new(SchedulePolicy::default()))
            .unwrap();
        crate::test_runtime::set_monotonic_ns(0);
        system.bring_cpu_online(cpu.as_mut()).unwrap();
        let contender = system
            .create_thread(ThreadSpec::new(SchedulePolicy::default()))
            .unwrap();
        system.make_ready(contender.id()).unwrap();
        system.enqueue(cpu.as_mut(), contender.id()).unwrap();

        crate::test_runtime::set_scheduler_ns_for_cpu(cpu.owner().as_u32(), 10_000);
        let clock = cpu.update_rq_clock();
        let monotonic_now = MonotonicInstant::from_nanos(100).unwrap();

        assert_eq!(
            cpu.as_mut().next_oneshot_deadline(clock, monotonic_now),
            MonotonicDeadline::from_nanos(BALANCE_INTERVAL_NS),
            "an unrelated runqueue-clock epoch must not expire the periodic balance clockevent"
        );
        assert!(
            !cpu.remote().needs_reschedule(),
            "a future monotonic balance deadline must not publish scheduler work"
        );
    }

    #[test]
    fn overdue_fair_balance_deadline_becomes_sticky_owner_work() {
        let system = TaskSystem::new(TaskSystemConfig::new(1)).unwrap();
        let mut cpu = system.create_cpu_local(CpuId::new(0)).unwrap();
        system
            .install_bootstrap_thread(cpu.as_mut(), ThreadSpec::new(SchedulePolicy::default()))
            .unwrap();
        system.bring_cpu_online(cpu.as_mut()).unwrap();
        let contender = system
            .create_thread(ThreadSpec::new(SchedulePolicy::default()))
            .unwrap();
        system.make_ready(contender.id()).unwrap();
        system.enqueue(cpu.as_mut(), contender.id()).unwrap();
        let deadline = cpu
            .remote()
            .fair_balance_deadline()
            .expect("online fair balancing must own a monotonic deadline");
        let clock = cpu.update_rq_clock();

        let next = cpu.as_mut().next_oneshot_deadline(
            clock,
            MonotonicInstant::from_nanos(deadline.as_nanos()).unwrap(),
        );
        assert!(
            next.is_none_or(|next| next > deadline),
            "an overdue balance event must not be rearmed at timer resolution: {next:?}"
        );
        assert!(
            cpu.remote().needs_reschedule(),
            "the due deadline must remain visible as scheduler work"
        );
    }

    #[test]
    fn load_summary_reader_does_not_wait_for_stalled_writer() {
        let remote = CpuRemote::create(CpuId::new(0), TaskSystemConfig::new(1));
        remote.set_load_summary_sequence_for_test(1);

        let reader_remote = Arc::clone(&remote);
        let (started_tx, started_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let reader = thread::spawn(move || {
            started_tx.send(()).unwrap();
            let summary = reader_remote.try_load_summary();
            finished_tx.send(summary).unwrap();
        });

        started_rx.recv().unwrap();
        let completed_while_writer_stalled =
            finished_rx.recv_timeout(Duration::from_millis(100)).is_ok();

        // Always release the old implementation's unbounded reader so a red
        // result cannot leak a host thread or hang the test process.
        remote.set_load_summary_sequence_for_test(2);
        reader.join().unwrap();

        assert!(
            completed_while_writer_stalled,
            "remote balancing must not spin forever behind a stalled owner writer"
        );
    }

    #[test]
    fn polling_idle_owner_observes_work_without_a_physical_ipi() {
        let remote = CpuRemote::create(CpuId::new(1), TaskSystemConfig::new(2));
        assert!(remote.mark_online());
        crate::test_runtime::configure_scheduler_ipi(RuntimeStatus::Success);

        assert!(
            remote.prepare_idle_wait(),
            "the empty owner must publish polling before the producer races"
        );
        assert!(remote.kick_scheduler_work());

        assert_eq!(
            crate::test_runtime::scheduler_ipi_send_count(),
            0,
            "a polling owner will observe sticky work in its final recheck"
        );
        assert!(remote.needs_reschedule());
        remote.finish_idle_wait();
    }

    #[test]
    fn logical_scheduler_work_does_not_claim_runtime_doorbell_ownership() {
        let remote = CpuRemote::create(CpuId::new(1), TaskSystemConfig::new(2));
        assert!(remote.mark_online());
        crate::test_runtime::configure_scheduler_ipi(RuntimeStatus::Success);

        assert!(remote.kick_scheduler_work());
        assert!(remote.kick_scheduler_work());
        assert_eq!(
            crate::test_runtime::scheduler_ipi_send_count(),
            2,
            "ax-task must forward every non-polling remote publication so the runtime doorbell can own physical-edge coalescing"
        );

        let _ = remote.take_preempt_requested();
        assert!(remote.kick_scheduler_work());
        assert_eq!(
            crate::test_runtime::scheduler_ipi_send_count(),
            3,
            "claiming logical scheduler work must remain independent of physical delivery state"
        );
    }

    #[test]
    fn request_published_between_claim_and_ack_remains_pending() {
        let remote = CpuRemote::create(CpuId::new(0), TaskSystemConfig::new(1));
        assert!(remote.mark_online());

        remote.request_reschedule();
        let first = remote.claim_scheduler_request();
        assert!(first.preempt_requested());

        remote.request_reschedule();
        remote.acknowledge_scheduler_request(first);
        assert!(
            remote.needs_reschedule(),
            "acknowledging an older rq transaction must not consume a concurrent publication"
        );

        let second = remote.claim_scheduler_request();
        assert!(second.preempt_requested());
        remote.acknowledge_scheduler_request(second);
        assert!(!remote.needs_reschedule());
    }

    #[test]
    fn owner_work_claim_does_not_manufacture_a_preemption_request() {
        let remote = CpuRemote::create(CpuId::new(0), TaskSystemConfig::new(1));
        assert!(remote.mark_online());

        remote.request_scheduler_work();
        let request = remote.claim_scheduler_request();
        assert!(!request.preempt_requested());
        remote.acknowledge_scheduler_request(request);
        assert!(!remote.needs_reschedule());
    }

    #[test]
    fn inactive_cpu_accepts_owner_delivery_but_rejects_new_placement() {
        let remote = CpuRemote::create(CpuId::new(1), TaskSystemConfig::new(2));
        assert!(remote.mark_online());
        assert!(remote.try_deactivate());
        assert_eq!(remote.lifecycle_state(), CpuLifecycleState::Inactive);
        assert!(remote.is_online());
        assert!(!remote.accepts_placement());

        assert!(
            remote.begin_publication().is_none(),
            "placement publication must close at the inactive boundary"
        );
        let owner_delivery = remote
            .begin_owner_delivery()
            .expect("in-flight owner control must remain deliverable while inactive");
        assert!(
            !remote.try_begin_draining(),
            "final draining must wait for in-flight owner delivery"
        );
        remote.cancel_deactivation();
        drop(owner_delivery);
        assert_eq!(remote.lifecycle_state(), CpuLifecycleState::Online);

        assert!(remote.try_deactivate());
        assert!(remote.try_begin_draining());
        assert!(remote.begin_owner_delivery().is_none());
        remote.finish_offline();
        assert_eq!(remote.lifecycle_state(), CpuLifecycleState::Offline);
    }
}
