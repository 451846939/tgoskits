//! AxVM-owned per-CPU VM timer wheel.

extern crate alloc;

use alloc::boxed::Box;
#[cfg(all(
    not(feature = "rt-shared-wait-baseline"),
    not(feature = "rt-disable-timer-cache")
))]
use core::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
#[cfg(any(
    target_arch = "x86_64",
    target_arch = "aarch64",
    target_arch = "loongarch64"
))]
use core::sync::atomic::{AtomicUsize, Ordering};
#[cfg(any(
    target_arch = "x86_64",
    target_arch = "aarch64",
    target_arch = "loongarch64"
))]
use core::time::Duration;

use ax_kspin::SpinNoIrq;
use ax_lazyinit::LazyInit;
use ax_timer_list::{TimeValue, TimerEvent, TimerList};

use crate::host::{HostTime, default_host};

#[cfg(any(
    target_arch = "x86_64",
    target_arch = "aarch64",
    target_arch = "loongarch64"
))]
static TOKEN: AtomicUsize = AtomicUsize::new(0);

#[cfg(all(
    not(feature = "rt-shared-wait-baseline"),
    not(feature = "rt-disable-timer-cache")
))]
const NO_TIMER_DEADLINE: u64 = u64::MAX;

#[cfg(all(
    not(feature = "rt-shared-wait-baseline"),
    not(feature = "rt-disable-timer-cache")
))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CachedDeadlineAction {
    ProcessExpired,
    Rearm(u64),
}

#[cfg(all(
    not(feature = "rt-shared-wait-baseline"),
    not(feature = "rt-disable-timer-cache")
))]
#[inline(always)]
fn cached_deadline_action(now_ns: u128, deadline_ns: u64) -> CachedDeadlineAction {
    if now_ns < deadline_ns as u128 {
        CachedDeadlineAction::Rearm(deadline_ns)
    } else {
        CachedDeadlineAction::ProcessExpired
    }
}

struct VmTimerEvent {
    #[cfg(any(target_arch = "x86_64", target_arch = "loongarch64"))]
    token: usize,
    callback: Box<dyn FnOnce(TimeValue) + Send + 'static>,
}

impl VmTimerEvent {
    #[cfg(any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "loongarch64"
    ))]
    fn new<F>(token: usize, callback: F) -> Self
    where
        F: FnOnce(TimeValue) + Send + 'static,
    {
        #[cfg(not(any(target_arch = "x86_64", target_arch = "loongarch64")))]
        let _ = token;
        Self {
            #[cfg(any(target_arch = "x86_64", target_arch = "loongarch64"))]
            token,
            callback: Box::new(callback),
        }
    }
}

impl TimerEvent for VmTimerEvent {
    fn callback(self, now: TimeValue) {
        (self.callback)(now);
    }
}

#[ax_percpu::def_percpu]
static TIMER_LIST: LazyInit<SpinNoIrq<TimerList<VmTimerEvent>>> = LazyInit::new();

#[cfg(all(
    not(feature = "rt-shared-wait-baseline"),
    not(feature = "rt-disable-timer-cache")
))]
#[ax_percpu::def_percpu]
static NEXT_TIMER_DEADLINE_NS: AtomicU64 = AtomicU64::new(NO_TIMER_DEADLINE);

#[cfg(any(
    target_arch = "x86_64",
    target_arch = "aarch64",
    target_arch = "loongarch64"
))]
pub(crate) fn register_timer(
    deadline_ns: u64,
    callback: Box<dyn FnOnce(Duration) + Send + 'static>,
) -> usize {
    let token = TOKEN.fetch_add(1, Ordering::Relaxed);
    let next_deadline = {
        // SAFETY: The timer list is initialized for each CPU before vCPU tasks
        // are spawned and VM timer callbacks are registered.
        let timer_list = unsafe { TIMER_LIST.current_ref_mut_raw() };
        let mut timers = timer_list.lock();
        timers.set(
            TimeValue::from_nanos(deadline_ns),
            VmTimerEvent::new(token, callback),
        );
        let next_deadline = timers.next_deadline();
        cache_next_deadline(next_deadline);
        next_deadline
    };
    rearm_host_timer(next_deadline);
    token
}

#[cfg(any(target_arch = "x86_64", target_arch = "loongarch64"))]
pub(crate) fn cancel_timer(token: usize) {
    let next_deadline = {
        // SAFETY: The timer list is initialized for each CPU before VM timer
        // callbacks are registered or cancelled.
        let timer_list = unsafe { TIMER_LIST.current_ref_mut_raw() };
        let mut timers = timer_list.lock();
        timers.cancel(|event| event.token == token);
        let next_deadline = timers.next_deadline();
        cache_next_deadline(next_deadline);
        next_deadline
    };
    rearm_host_timer(next_deadline);
}

pub(crate) fn check_events() {
    // SAFETY: Called from a vCPU task pinned to a CPU whose timer list was
    // initialized during AxVM host initialization.
    let timer_list = unsafe { TIMER_LIST.current_ref_mut_raw() };

    #[cfg(all(
        not(feature = "rt-shared-wait-baseline"),
        not(feature = "rt-disable-timer-cache")
    ))]
    let mut now = {
        let deadline_ns = cached_next_deadline_ns();
        if deadline_ns == NO_TIMER_DEADLINE {
            crate::runtime::rt_trace::trace_timer_check(
                crate::runtime::rt_trace::TimerCheckPath::NoDeadline,
            );
            return;
        }
        let now = default_host().monotonic_time();
        match cached_deadline_action(now.as_nanos(), deadline_ns) {
            CachedDeadlineAction::ProcessExpired => {}
            CachedDeadlineAction::Rearm(deadline_ns) => {
                crate::runtime::rt_trace::trace_timer_check(
                    crate::runtime::rt_trace::TimerCheckPath::NotDue,
                );
                // A one-shot timer may fire slightly before its programmed
                // deadline. The interrupt consumes that one-shot, so leaving
                // it unarmed here can defer the VM timer until an unrelated
                // event checks the timer wheel again.
                default_host().set_oneshot_timer(deadline_ns);
                return;
            }
        }
        now
    };

    loop {
        #[cfg(any(
            feature = "rt-shared-wait-baseline",
            feature = "rt-disable-timer-cache"
        ))]
        let now = default_host().monotonic_time();
        let (expired, next_deadline) = {
            let mut timers = timer_list.lock();
            let expired = timers.expire_one(now);
            let next_deadline = timers.next_deadline();
            cache_next_deadline(next_deadline);
            (expired, next_deadline)
        };
        crate::runtime::rt_trace::trace_timer_check(
            crate::runtime::rt_trace::TimerCheckPath::Locked {
                expired: expired.is_some(),
            },
        );
        if let Some((deadline, event)) = expired {
            trace!("handle VM timer event scheduled at {deadline:#?}");
            event.callback(now);
            #[cfg(all(
                not(feature = "rt-shared-wait-baseline"),
                not(feature = "rt-disable-timer-cache")
            ))]
            {
                now = default_host().monotonic_time();
            }
        } else {
            rearm_host_timer(next_deadline);
            break;
        }
    }
}

#[cfg(any(
    feature = "rt-shared-wait-baseline",
    feature = "rt-disable-timer-cache"
))]
#[inline(always)]
fn cache_next_deadline(_next_deadline: Option<TimeValue>) {}

#[cfg(all(
    not(feature = "rt-shared-wait-baseline"),
    not(feature = "rt-disable-timer-cache")
))]
#[inline(always)]
fn cache_next_deadline(next_deadline: Option<TimeValue>) {
    let deadline_ns = next_deadline
        .map(|deadline| deadline.as_nanos().min((u64::MAX - 1) as u128) as u64)
        .unwrap_or(NO_TIMER_DEADLINE);
    // SAFETY: The per-CPU timer state is initialized before any VM timer can
    // be registered, and vCPU tasks remain pinned to their selected CPU.
    unsafe { NEXT_TIMER_DEADLINE_NS.current_ref_raw() }.store(deadline_ns, AtomicOrdering::Release);
}

#[cfg(all(
    not(feature = "rt-shared-wait-baseline"),
    not(feature = "rt-disable-timer-cache")
))]
#[inline(always)]
fn cached_next_deadline_ns() -> u64 {
    // SAFETY: See `cache_next_deadline`; readers run on the same initialized
    // per-CPU timer state and use Acquire to observe a newly registered timer.
    unsafe { NEXT_TIMER_DEADLINE_NS.current_ref_raw() }.load(AtomicOrdering::Acquire)
}

fn rearm_host_timer(next_deadline: Option<TimeValue>) {
    #[cfg(target_arch = "loongarch64")]
    let _ = next_deadline;
    #[cfg(not(target_arch = "loongarch64"))]
    if let Some(deadline) = next_deadline {
        default_host().set_oneshot_timer(deadline.as_nanos() as u64);
    }
}

pub(crate) fn init_percpu() {
    info!("Initializing AxVM timer wheel...");
    // SAFETY: Called once per CPU during hypervisor initialization before this
    // CPU can register VM timers.
    let timer_list = unsafe { TIMER_LIST.current_ref_mut_raw() };
    timer_list.init_once(SpinNoIrq::new(TimerList::new()));
    #[cfg(target_arch = "loongarch64")]
    ax_std::os::arceos::modules::ax_task::register_timer_callback(|_| check_events());
}

#[cfg(test)]
mod tests {
    #[cfg(all(
        not(feature = "rt-shared-wait-baseline"),
        not(feature = "rt-disable-timer-cache")
    ))]
    use super::{CachedDeadlineAction, cached_deadline_action};

    #[cfg(all(
        not(feature = "rt-shared-wait-baseline"),
        not(feature = "rt-disable-timer-cache")
    ))]
    #[test]
    fn early_one_shot_interrupt_requires_rearm() {
        assert_eq!(
            cached_deadline_action(999, 1_000),
            CachedDeadlineAction::Rearm(1_000)
        );
    }

    #[cfg(all(
        not(feature = "rt-shared-wait-baseline"),
        not(feature = "rt-disable-timer-cache")
    ))]
    #[test]
    fn timer_is_processed_at_or_after_deadline() {
        assert_eq!(
            cached_deadline_action(1_000, 1_000),
            CachedDeadlineAction::ProcessExpired
        );
        assert_eq!(
            cached_deadline_action(1_001, 1_000),
            CachedDeadlineAction::ProcessExpired
        );
    }
}
