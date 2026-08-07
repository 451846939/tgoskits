//! PREEMPT_RT-style per-CPU soft-timer worker publication.
//!
//! The hrtimer base remains the only owner of timeout payload. This state is
//! equivalent to Linux's per-CPU timer-softirq pending bit plus the
//! `ktimers/%u` smpboot thread: hard IRQ only publishes a sticky event, while
//! the fixed worker drains timeout wakeups in task context.

use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use super::*;
use crate::IrqWaitCell;

const WORKER_UNINSTALLED: u8 = 0;
const WORKER_STARTING: u8 = 1;
const WORKER_INSTALLED: u8 = 2;

#[derive(Debug)]
pub(super) struct KtimerWorkerState {
    event: IrqWaitCell,
    worker_state: AtomicU8,
    worker_thread: AtomicU64,
}

impl KtimerWorkerState {
    pub(super) const fn new() -> Self {
        Self {
            event: IrqWaitCell::new(),
            worker_state: AtomicU8::new(WORKER_UNINSTALLED),
            worker_thread: AtomicU64::new(0),
        }
    }

    fn begin_install(&self) -> Result<(), TaskError> {
        self.worker_state
            .compare_exchange(
                WORKER_UNINSTALLED,
                WORKER_STARTING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map(|_| ())
            .map_err(|_| TaskError::InvalidConfiguration)
    }

    fn finish_install(&self, thread: ThreadId) {
        assert_ne!(
            thread.as_u64(),
            0,
            "ktimer worker must have a generation-bearing identity"
        );
        assert_eq!(
            self.worker_thread.compare_exchange(
                0,
                thread.as_u64(),
                Ordering::Release,
                Ordering::Acquire
            ),
            Ok(0),
            "ktimer worker identity was installed twice"
        );
        assert_eq!(
            self.worker_state.compare_exchange(
                WORKER_STARTING,
                WORKER_INSTALLED,
                Ordering::Release,
                Ordering::Acquire,
            ),
            Ok(WORKER_STARTING),
            "ktimer worker completed installation from an invalid state"
        );
    }

    fn cancel_install(&self) {
        assert_eq!(
            self.worker_state.compare_exchange(
                WORKER_STARTING,
                WORKER_UNINSTALLED,
                Ordering::AcqRel,
                Ordering::Acquire,
            ),
            Ok(WORKER_STARTING),
            "ktimer worker cancelled installation from an invalid state"
        );
    }

    fn worker_thread(&self) -> Option<ThreadId> {
        let raw = self.worker_thread.load(Ordering::Acquire);
        (raw != 0).then(|| ThreadId::from_parts(raw as u32, (raw >> 32) as u32))
    }

    fn is_quiescent_for_offline(&self) -> bool {
        match self.worker_state.load(Ordering::Acquire) {
            WORKER_UNINSTALLED => true,
            WORKER_INSTALLED => !self.event.is_pending(),
            WORKER_STARTING => false,
            state => task_runtime::fatal_invariant(0x4b54_0001, state as usize),
        }
    }

    const fn event(&self) -> &IrqWaitCell {
        &self.event
    }

    fn publish(&self) {
        let _notified = self.event.notify();
    }
}

impl CpuRemote {
    pub(crate) fn begin_ktimer_worker_install(&self) -> Result<(), TaskError> {
        self.ktimer.begin_install()
    }

    pub(crate) fn finish_ktimer_worker_install(&self, thread: ThreadId) {
        self.ktimer.finish_install(thread);
    }

    pub(crate) fn cancel_ktimer_worker_install(&self) {
        self.ktimer.cancel_install();
    }

    pub(crate) fn publish_ktimer_work(&self) {
        self.ktimer.publish();
    }

    pub(crate) const fn ktimer_event(&self) -> &IrqWaitCell {
        self.ktimer.event()
    }

    pub(crate) fn ktimer_worker(&self) -> Option<ThreadId> {
        self.ktimer.worker_thread()
    }

    pub(in crate::system::cpu) fn ktimer_is_quiescent_for_offline(&self) -> bool {
        self.ktimer.is_quiescent_for_offline()
    }
}
