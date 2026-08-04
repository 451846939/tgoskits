//! Task-context wait queues built on the generation-checked park handshake.

use alloc::collections::VecDeque;
use core::{
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use crate::{
    CurrentParkStart, TaskError, ThreadId, ThreadWakeHandle,
    facade::{acquire_blocking_permit, begin_current_park_with_permit},
    lock::PreemptTicketLock,
    runtime::task_runtime,
};

/// Sleeps the calling scheduler thread for at least `duration`.
#[track_caller]
pub fn sleep(duration: Duration) {
    let deadline_ns = deadline_after(duration);
    sleep_until_ns(deadline_ns);
}

/// Sleeps until an absolute deadline measured against the monotonic clock.
#[track_caller]
pub fn sleep_until(deadline: Duration) {
    let deadline_ns = deadline.as_nanos().min(u64::MAX as u128) as u64;
    sleep_until_ns(deadline_ns);
}

/// A FIFO of scheduler threads that may sleep in ordinary task context.
///
/// This object intentionally has no hard-IRQ notification API. IRQ producers
/// should wake one fixed service thread through [`crate::IrqWaitCell`], then let
/// that thread fan out notifications here.
#[derive(Debug)]
pub struct WaitQueue {
    waiters: PreemptTicketLock<VecDeque<Waiter>>,
    notification_generation: AtomicU64,
}

impl WaitQueue {
    /// Creates an empty wait queue suitable for static initialization.
    pub const fn new() -> Self {
        Self {
            waiters: PreemptTicketLock::new(VecDeque::new()),
            notification_generation: AtomicU64::new(0),
        }
    }

    /// Blocks the current thread until one task-context notification selects it.
    #[track_caller]
    pub fn wait(&self) {
        self.wait_once(None)
            .expect("wait queue park must satisfy scheduler invariants");
    }

    /// Blocks until `condition` observes true.
    ///
    /// The predicate runs in ordinary task context without the internal waiter
    /// lock. A producer must publish the state observed by `condition` before
    /// notifying this queue. The notification generation closes the interval
    /// between the predicate check and waiter insertion without calling
    /// arbitrary code from a scheduler-sensitive critical section.
    #[track_caller]
    pub fn wait_until<F>(&self, condition: F)
    where
        F: Fn() -> bool,
    {
        self.try_wait_until(condition)
            .expect("conditional wait must satisfy scheduler invariants");
    }

    /// Fallible form of [`Self::wait_until`] for runtime and OS glue.
    ///
    /// The predicate follows the same publish-before-notify contract as
    /// [`Self::wait_until`].
    ///
    /// # Errors
    ///
    /// Returns [`TaskError::UnsafeContext`] in hard IRQ context and propagates
    /// scheduler, timer-capacity, and runtime capability failures.
    pub fn try_wait_until<F>(&self, condition: F) -> Result<(), TaskError>
    where
        F: Fn() -> bool,
    {
        loop {
            if self.wait_once_if(None, &condition)? {
                return Ok(());
            }
        }
    }

    /// Blocks until notification or the relative timeout elapses.
    ///
    /// Returns `true` only when the timer won removal from the queue. A racing
    /// notification that already selected this waiter wins over the deadline.
    #[track_caller]
    pub fn wait_timeout(&self, timeout: Duration) -> bool {
        let deadline_ns = deadline_after(timeout);
        loop {
            if task_runtime::monotonic_ns() >= deadline_ns {
                return true;
            }
            let outcome = self
                .wait_once(Some(deadline_ns))
                .expect("timed wait must satisfy scheduler invariants");
            if outcome == WaitOutcome::Notified {
                return false;
            }
            if task_runtime::monotonic_ns() >= deadline_ns {
                return true;
            }
        }
    }

    /// Blocks until `condition` becomes true or the relative timeout elapses.
    ///
    /// Returns `true` for timeout and `false` when the condition wins.
    #[track_caller]
    pub fn wait_timeout_until<F>(&self, timeout: Duration, condition: F) -> bool
    where
        F: Fn() -> bool,
    {
        self.wait_until_deadline(Duration::from_nanos(deadline_after(timeout)), condition)
    }

    /// Blocks until `condition` becomes true or an absolute deadline elapses.
    ///
    /// `deadline` is measured against the runtime monotonic clock. Unlike a
    /// relative timeout loop, this method never rebases the deadline after a
    /// spurious wake, so repeated notifications cannot extend the wait.
    /// Returns `true` for timeout and `false` when the condition wins.
    #[track_caller]
    pub fn wait_until_deadline<F>(&self, deadline: Duration, condition: F) -> bool
    where
        F: Fn() -> bool,
    {
        let deadline_ns = deadline.as_nanos().min(u64::MAX as u128) as u64;
        loop {
            if task_runtime::monotonic_ns() >= deadline_ns {
                return !condition();
            }
            let condition_met = self
                .wait_once_if(Some(deadline_ns), &condition)
                .unwrap_or_else(|error| {
                    panic!("timed conditional wait must satisfy scheduler invariants: {error:?}")
                });
            if condition_met {
                return false;
            }
        }
    }

    /// Selects and wakes the oldest waiter from ordinary task context.
    ///
    /// # Panics
    ///
    /// Panics in hard IRQ context. IRQ producers must use
    /// [`crate::IrqWaitCell`] to wake one fixed service thread.
    pub fn notify_one(&self) -> bool {
        assert_task_context_notification();
        let Some(waiter) = self.pop_front_task_context() else {
            return false;
        };
        let _result = waiter.wake.wake_from_task();
        true
    }

    /// Wakes every waiter, releasing the queue lock before each direct wake.
    pub fn notify_all(&self) {
        while self.notify_one() {}
    }

    fn wait_once(&self, deadline_ns: Option<u64>) -> Result<WaitOutcome, TaskError> {
        self.wait_once_inner(deadline_ns, None)
    }

    fn wait_once_if(
        &self,
        deadline_ns: Option<u64>,
        condition: &dyn Fn() -> bool,
    ) -> Result<bool, TaskError> {
        match self.wait_once_inner(deadline_ns, Some(condition))? {
            WaitOutcome::Condition => Ok(true),
            WaitOutcome::Notified | WaitOutcome::OtherWake => Ok(false),
        }
    }

    fn wait_once_inner(
        &self,
        deadline_ns: Option<u64>,
        condition: Option<&dyn Fn() -> bool>,
    ) -> Result<WaitOutcome, TaskError> {
        // Validate sleepability before taking the queue's non-sleeping
        // publication lock. This permit cannot escape the park attempt.
        let permit = acquire_blocking_permit()?;
        let observed_generation = if let Some(condition) = condition {
            let generation = self.notification_generation.load(Ordering::Acquire);
            if condition() {
                return Ok(WaitOutcome::Condition);
            }
            Some(generation)
        } else {
            None
        };
        let park = {
            let mut waiters = self.waiters.lock();
            if observed_generation.is_some_and(|generation| {
                self.notification_generation.load(Ordering::Acquire) != generation
            }) {
                return Ok(WaitOutcome::OtherWake);
            }
            let mut park = match begin_current_park_with_permit(&permit)? {
                CurrentParkStart::Notified => return Ok(WaitOutcome::OtherWake),
                CurrentParkStart::Prepared(park) => park,
            };
            let thread = park.thread_id();
            waiters.push_back(Waiter::new(thread, park.wake_handle()));
            if let Some(deadline_ns) = deadline_ns
                && let Err(error) = park.arm_deadline(deadline_ns)
            {
                remove_waiter(&mut waiters, thread);
                park.cancel()?;
                return Err(error);
            }
            park
        };
        let thread = park.thread_id();

        if let Err(error) = park.commit() {
            remove_waiter(&mut self.waiters.lock(), thread);
            return Err(error);
        }
        let removed = remove_waiter(&mut self.waiters.lock(), thread);
        Ok(if removed {
            WaitOutcome::OtherWake
        } else {
            WaitOutcome::Notified
        })
    }

    fn pop_front_task_context(&self) -> Option<Waiter> {
        let mut waiters = self.waiters.lock();
        self.notification_generation.fetch_add(1, Ordering::Release);
        waiters.pop_front()
    }
}

fn assert_task_context_notification() {
    assert!(
        !task_runtime::in_hard_irq(),
        "WaitQueue notification is task-context-only; use IrqWaitCell from hard IRQ"
    );
}

impl Default for WaitQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
struct Waiter {
    thread: ThreadId,
    wake: ThreadWakeHandle,
}

impl Waiter {
    fn new(thread: ThreadId, wake: ThreadWakeHandle) -> Self {
        Self { thread, wake }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WaitOutcome {
    Condition,
    Notified,
    OtherWake,
}

fn remove_waiter(waiters: &mut VecDeque<Waiter>, thread: ThreadId) -> bool {
    let Some(index) = waiters.iter().position(|waiter| waiter.thread == thread) else {
        return false;
    };
    waiters.remove(index);
    true
}

fn deadline_after(timeout: Duration) -> u64 {
    let timeout_ns = timeout.as_nanos().min(u64::MAX as u128) as u64;
    task_runtime::monotonic_ns().saturating_add(timeout_ns)
}

fn sleep_until_ns(deadline_ns: u64) {
    let queue = WaitQueue::new();
    loop {
        let now_ns = task_runtime::monotonic_ns();
        if now_ns >= deadline_ns {
            return;
        }
        if queue.wait_timeout(Duration::from_nanos(deadline_ns - now_ns)) {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::boxed::Box;

    use super::*;
    use crate::{CpuId, SchedulePolicy, TaskSystem, TaskSystemConfig, ThreadSpec, WakeResult};

    struct InstalledTaskHandles;

    impl InstalledTaskHandles {
        fn new(
            system: core::pin::Pin<&TaskSystem>,
            cpu: core::pin::Pin<&mut crate::CpuLocal>,
        ) -> Self {
            crate::test_runtime::install_task_handles(
                (system.get_ref() as *const TaskSystem).expose_provenance(),
                // SAFETY: the fixture publishes this pointer only while the
                // pinned CPU object and owning task system remain alive.
                (unsafe { core::pin::Pin::get_unchecked_mut(cpu) } as *mut crate::CpuLocal)
                    .expose_provenance(),
            );
            Self
        }
    }

    impl Drop for InstalledTaskHandles {
        fn drop(&mut self) {
            crate::test_runtime::clear_task_handles();
        }
    }

    #[test]
    fn waiter_publication_uses_one_scheduler_owner_transaction() {
        let system = Box::pin(TaskSystem::new(TaskSystemConfig::new(1)).unwrap());
        let mut cpu = system.create_cpu_local(CpuId::new(0)).unwrap();
        let running = system
            .install_bootstrap_thread(cpu.as_mut(), ThreadSpec::new(SchedulePolicy::default()))
            .unwrap();
        system.bring_cpu_online(cpu.as_mut()).unwrap();
        let _runtime_handles = InstalledTaskHandles::new(system.as_ref(), cpu.as_mut());
        assert_eq!(running.wake_handle().wake_from_task(), WakeResult::Notified);
        crate::test_runtime::reset_cpu_handle_reads();

        assert_eq!(
            WaitQueue::new().wait_once(None).unwrap(),
            WaitOutcome::OtherWake
        );

        assert_eq!(
            crate::test_runtime::cpu_owner_claims(),
            1,
            "current identity capture and waiter park publication must be one scheduler \
             transaction"
        );
    }

    #[test]
    fn elapsed_conditional_deadline_checks_predicate_outside_the_waiter_lock() {
        crate::test_runtime::reset_irq_state();
        crate::test_runtime::reset_preempt_state();
        crate::test_runtime::set_monotonic_ns(10);
        let queue = WaitQueue::new();
        let predicate_can_take_unrelated_state = core::cell::Cell::new(false);

        let timed_out = queue.wait_until_deadline(Duration::from_nanos(10), || {
            predicate_can_take_unrelated_state.set(queue.waiters.try_lock().is_some());
            false
        });

        assert!(timed_out);
        assert!(
            predicate_can_take_unrelated_state.get(),
            "wait predicates must run without the scheduler-sensitive waiter lock"
        );
        assert_eq!(crate::test_runtime::active_irq_guards(), 0);
        assert_eq!(crate::test_runtime::active_preempt_guards(), 0);
    }

    #[test]
    fn notification_removal_wins_the_timeout_cleanup_race() {
        let system = TaskSystem::new(TaskSystemConfig::new(1)).unwrap();
        let thread = system
            .create_thread(ThreadSpec::new(SchedulePolicy::default()))
            .unwrap();
        let queue = WaitQueue::new();
        queue
            .waiters
            .lock()
            .push_back(Waiter::new(thread.id(), thread.wake_handle()));

        assert!(queue.notify_one());
        assert!(!remove_waiter(&mut queue.waiters.lock(), thread.id()));
    }

    #[test]
    fn timeout_cleanup_removes_an_unselected_waiter() {
        let system = TaskSystem::new(TaskSystemConfig::new(1)).unwrap();
        let thread = system
            .create_thread(ThreadSpec::new(SchedulePolicy::default()))
            .unwrap();
        let queue = WaitQueue::new();
        queue
            .waiters
            .lock()
            .push_back(Waiter::new(thread.id(), thread.wake_handle()));

        assert!(remove_waiter(&mut queue.waiters.lock(), thread.id()));
        assert!(!queue.notify_one());
    }

    #[test]
    fn hard_irq_notification_is_rejected_instead_of_silently_losing_the_wake() {
        let queue = WaitQueue::new();
        crate::test_runtime::set_hard_irq(true);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| queue.notify_one()));
        crate::test_runtime::set_hard_irq(false);

        assert!(result.is_err());
    }
}
