//! Checked thread lifecycle transitions.

use core::sync::atomic::{AtomicU8, Ordering};

use crate::TaskError;

/// Observable lifecycle state of a thread.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThreadState {
    /// Allocated but not admitted to a run queue.
    New     = 0,
    /// Eligible to run or already present in a run queue.
    Ready   = 1,
    /// Currently executing on a CPU.
    Running = 2,
    /// Publishing a block operation while racing with wake-up.
    Parking = 3,
    /// Asleep on a wait object.
    Blocked = 4,
    /// A wake operation won the block/wake race.
    Waking  = 5,
    /// Execution has terminated and resources await reaping.
    Exited  = 6,
}

/// Single atomic publication for task lifecycle and wake/schedule races.
#[derive(Debug)]
pub(crate) struct ThreadLifecycle {
    state: AtomicU8,
}

impl ThreadLifecycle {
    pub(crate) const fn new() -> Self {
        Self {
            state: AtomicU8::new(ThreadState::New as u8),
        }
    }

    pub(crate) fn state(&self) -> ThreadState {
        decode_state(self.state.load(Ordering::Acquire))
    }

    pub(crate) fn transition(&self, next: ThreadState) -> Result<(), TaskError> {
        let current = self.state();
        if !transition_is_valid(current, next) {
            return Err(TaskError::InvalidTransition {
                from: current,
                to: next,
            });
        }
        self.state
            .compare_exchange(
                current as u8,
                next as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map(|_| ())
            .map_err(|observed| TaskError::InvalidTransition {
                from: decode_state(observed),
                to: next,
            })
    }
}

pub(crate) const fn decode_state(state: u8) -> ThreadState {
    match state {
        0 => ThreadState::New,
        1 => ThreadState::Ready,
        2 => ThreadState::Running,
        3 => ThreadState::Parking,
        4 => ThreadState::Blocked,
        5 => ThreadState::Waking,
        6 => ThreadState::Exited,
        _ => panic!("invalid thread lifecycle publication"),
    }
}

pub(crate) const fn transition_is_valid(from: ThreadState, to: ThreadState) -> bool {
    matches!(
        (from, to),
        (ThreadState::New, ThreadState::Ready | ThreadState::Exited)
            | (
                ThreadState::Ready,
                ThreadState::Running | ThreadState::Exited
            )
            | (
                ThreadState::Running,
                ThreadState::Ready
                    | ThreadState::Parking
                    | ThreadState::Blocked
                    | ThreadState::Exited
            )
            | (
                ThreadState::Parking,
                ThreadState::Running
                    | ThreadState::Blocked
                    | ThreadState::Waking
                    | ThreadState::Ready
            )
            | (
                ThreadState::Blocked,
                ThreadState::Waking | ThreadState::Exited
            )
            | (
                ThreadState::Waking,
                ThreadState::Ready | ThreadState::Exited
            )
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_the_documented_wake_transition() {
        let lifecycle = ThreadLifecycle::new();
        lifecycle.transition(ThreadState::Ready).unwrap();
        lifecycle.transition(ThreadState::Running).unwrap();
        lifecycle.transition(ThreadState::Parking).unwrap();
        lifecycle.transition(ThreadState::Waking).unwrap();
        lifecycle.transition(ThreadState::Ready).unwrap();
        assert_eq!(lifecycle.state(), ThreadState::Ready);
    }

    #[test]
    fn rejects_ready_to_blocked_shortcut() {
        assert!(!transition_is_valid(
            ThreadState::Ready,
            ThreadState::Blocked
        ));
    }
}
