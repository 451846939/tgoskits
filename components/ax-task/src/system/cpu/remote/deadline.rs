//! IRQ-safe per-CPU task-deadline base.
//!
//! Linux hrtimer bases are per CPU, but their raw lock is reachable from a
//! remote `task_rq_lock()` migration transaction. Keeping this state below
//! [`CpuRemote`] provides the same ownership boundary: the local timer IRQ and
//! soft-timer worker remain the only consumers, while a remote wake migration
//! may cancel an old registration before moving its rq bandwidth.

use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SchedulerDeadlinePublicationState {
    pub(crate) deadline: Option<MonotonicDeadline>,
}

#[derive(Debug)]
pub(crate) struct CpuDeadlineState {
    pub(crate) queue: TaskDeadlineQueue,
    pub(crate) expired_buffer: Vec<ExpiredTaskDeadline>,
    pub(crate) expired_count: usize,
    pub(crate) generation: u64,
    pub(crate) publication: Option<SchedulerDeadlinePublicationState>,
    #[cfg(test)]
    pub(crate) expire_passes: usize,
}

impl CpuDeadlineState {
    pub(crate) fn new(config: TaskSystemConfig) -> Self {
        Self {
            queue: TaskDeadlineQueue::new(config.thread_capacity()),
            expired_buffer: vec![ExpiredTaskDeadline::EMPTY; config.batch_limit()],
            expired_count: 0,
            generation: 0,
            publication: None,
            #[cfg(test)]
            expire_passes: 0,
        }
    }

    pub(crate) fn owns_buffered_expiration(&self, registration: &TaskDeadlineRegistration) -> bool {
        self.expired_buffer[..self.expired_count]
            .iter()
            .copied()
            .any(|event| {
                event.thread() == Some(registration.thread())
                    && event.token() == registration.token()
                    && event.deadline() == Some(registration.deadline())
                    && event.kind() == Some(registration.kind())
            })
    }
}
