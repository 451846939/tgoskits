//! Pinned owner-CPU scheduler state.

mod clock;
mod dispatch;
mod load;
mod local;
mod remote;
mod snapshot;

use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};
use core::{
    marker::{PhantomData, PhantomPinned},
    ops::Deref,
    pin::Pin,
    ptr::NonNull,
    sync::atomic::{AtomicBool, AtomicU8, AtomicU16, AtomicU64, AtomicUsize, Ordering},
};

pub(crate) use clock::{RunQueueClock, RunQueueClockSnapshot};
pub(crate) use dispatch::{
    CurrentDispatch, CurrentDispatchState, CurrentSchedule, DispatchCharge, DispatchRole,
    SwitchHandoff,
};
pub use load::{CpuLoadSummary, DeadlineBandwidthSnapshot, SchedulingClass};
use load::{
    LOAD_SUMMARY_READ_RETRIES, SUMMARY_CLASS_MASK, SUMMARY_CURRENT_CLASS_SHIFT,
    SUMMARY_CURRENT_PRESENT, SUMMARY_OVERLOADED, SUMMARY_PUSHABLE_CLASS_SHIFT,
    SUMMARY_PUSHABLE_PRESENT,
};
pub use local::CpuLocal;
use local::earliest;
#[cfg(feature = "qperf-metrics")]
pub(crate) use remote::WakePreemptionDecision;
pub use remote::{CpuLifecycleState, CpuLocalOwnerBorrow, CpuRemote};
pub(crate) use remote::{
    CpuRemotePublication, CpuRunQueueState, IdlePullReservation, PreparedMigrationDelivery,
};
pub use snapshot::CpuSnapshot;

use crate::{
    CpuId, DeadlineAdmission, FairMode, RtBandwidth, RunQueue, SchedulePolicy, SchedulingEntity,
    SchedulingKey, TaskError, TaskSystemConfig, ThreadHandle, ThreadId, ThreadState,
    inbox::{InboxKind, InboxMessage, InboxNode, PublishResult, SchedulerInbox},
    lock::{IrqScope, IrqTicketGuard, IrqTicketLock},
    runtime::{
        MonotonicDeadline, MonotonicInstant, RuntimeCpuId, RuntimeStatus, TaskDeadlineUpdate,
        task_runtime,
    },
    thread::ThreadCore,
    timer::{
        ExpiredTaskDeadline, TaskDeadlineExpireBatch, TaskDeadlineExpireRequest, TaskDeadlineQueue,
        TaskDeadlineRegistration,
    },
};
