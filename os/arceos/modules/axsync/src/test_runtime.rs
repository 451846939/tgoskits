//! Per-unit-test-binary task and lock runtime symbols.

use core::cell::Cell;
#[cfg(feature = "lockdep")]
use core::cell::RefCell;

use ax_task::{
    CpuId, CpuRemote, TaskSystem, impl_trait as impl_task_runtime,
    runtime::{
        AddressSpaceActivation, AddressSpaceDestroyOutcome, AddressSpaceHandle,
        AddressSpaceMembarrierState, AddressSpaceReclaimArmOutcome, ContextSwitch,
        ContextThreadBinding, CpuRemoteHandle, CurrentCpuLocalHandle, CurrentCpuOwnerHandles,
        CurrentThreadPublication, ExecutionContextHandle, IrqGuardToken, KernelContextRequest,
        MembarrierRegistration, MembarrierRegistrationPhase, PreemptGuardToken, RqClockSample,
        RuntimeCpuId, RuntimeHandleResult, RuntimeMembarrierAction, RuntimeScheduleOrigin,
        RuntimeSchedulerEntry, RuntimeSchedulerReturn, RuntimeStatus, SchedSwitchRecord,
        SchedulerDeadlineUpdate, StackHandle, StackRequest, TaskRuntime, TaskSystemHandle,
        TlsHandle, TlsRequest, UserContextRequest,
    },
};

struct UnitTestKernelGuard;
struct UnitTestTaskRuntime;

std::thread_local! {
    static TASK_SYSTEM: Cell<usize> = const { Cell::new(0) };
    static CPU_LOCAL: Cell<usize> = const { Cell::new(0) };
    static CPU_REMOTE: Cell<usize> = const { Cell::new(0) };
    static CURRENT_PUBLICATION: Cell<CurrentThreadPublication> =
        const { Cell::new(CurrentThreadPublication::NONE) };
    static PREEMPT_DEPTH: Cell<usize> = const { Cell::new(0) };
    static SCHEDULE_CONTEXT_SAFE: Cell<bool> = const { Cell::new(true) };
    static IRQ_GUARD_ENTRIES: Cell<usize> = const { Cell::new(0) };
    static CPU_OWNER_HANDLE_READS: Cell<usize> = const { Cell::new(0) };
    static PREEMPT_GUARD_ENTRIES: Cell<usize> = const { Cell::new(0) };
}

#[cfg(feature = "lockdep")]
std::thread_local! {
    static HELD_LOCKS: RefCell<ax_lockdep::HeldLockStack> =
        const { RefCell::new(ax_lockdep::HeldLockStack::new()) };
}

#[ax_crate_interface::impl_interface]
impl ax_kernel_guard::KernelGuardIf for UnitTestKernelGuard {
    fn hardirq_enter() {}

    fn hardirq_exit() {}

    fn disable_preempt() {
        PREEMPT_DEPTH.with(|depth| {
            depth.set(
                depth
                    .get()
                    .checked_add(1)
                    .expect("test preempt depth overflow"),
            );
        });
    }

    fn enable_preempt() {
        PREEMPT_DEPTH.with(|depth| {
            depth.set(
                depth
                    .get()
                    .checked_sub(1)
                    .expect("test preempt enable without disable"),
            );
        });
    }

    fn enable_preempt_from_irq_return() {
        Self::enable_preempt();
    }
}

impl_task_runtime! {
    impl TaskRuntime for UnitTestTaskRuntime {
        unsafe fn task_system_handle() -> TaskSystemHandle {
            // SAFETY: install/clear bracket the pinned fixture TaskSystem.
            unsafe { TaskSystemHandle::from_raw(TASK_SYSTEM.with(Cell::get)) }
        }
        unsafe fn current_cpu_owner_handles() -> CurrentCpuOwnerHandles {
            CPU_OWNER_HANDLE_READS.with(|reads| reads.set(reads.get() + 1));
            let local = CPU_LOCAL.with(Cell::get);
            let remote = CPU_REMOTE.with(Cell::get);
            // SAFETY: install/clear publish the paired owner and remote handles
            // for modeled CPU 0 and keep their TaskSystem alive.
            unsafe {
                CurrentCpuOwnerHandles::new(
                    RuntimeCpuId::new(0),
                    CurrentCpuLocalHandle::from_raw(local),
                    CpuRemoteHandle::from_raw(remote),
                )
            }
        }
        unsafe fn current_cpu_remote_handle() -> CpuRemoteHandle {
            // SAFETY: install/clear bracket the fixture TaskSystem that owns
            // this cached current-CPU endpoint.
            unsafe { CpuRemoteHandle::from_raw(CPU_REMOTE.with(Cell::get)) }
        }
        unsafe fn current_thread_publication() -> CurrentThreadPublication {
            CURRENT_PUBLICATION.with(Cell::get)
        }
        unsafe fn cpu_remote_handle(cpu: RuntimeCpuId) -> CpuRemoteHandle {
            let raw = TASK_SYSTEM.with(Cell::get);
            if raw == 0 {
                return CpuRemoteHandle::NONE;
            }
            // SAFETY: install/clear keep the pinned fixture TaskSystem alive.
            let system = unsafe { &*core::ptr::with_exposed_provenance::<TaskSystem>(raw) };
            system
                .cpu_remote(CpuId::new(cpu.as_u32()))
                .map_or(CpuRemoteHandle::NONE, |remote| {
                    // SAFETY: TaskSystem owns the Arc-backed endpoint while the
                    // fixture handle remains installed.
                    unsafe {
                        CpuRemoteHandle::from_raw(
                            (remote as *const CpuRemote).expose_provenance(),
                        )
                    }
                })
        }
        unsafe fn current_cpu_id() -> RuntimeCpuId { RuntimeCpuId::new(0) }
        fn prepare_cpu_online(_cpu: RuntimeCpuId) -> RuntimeStatus { RuntimeStatus::Success }
        fn prepare_cpu_offline(_cpu: RuntimeCpuId) -> RuntimeStatus { RuntimeStatus::Success }
        fn irq_guard_enter() -> IrqGuardToken {
            IRQ_GUARD_ENTRIES.with(|entries| entries.set(entries.get() + 1));
            // SAFETY: this single-CPU test runtime models one balanced token.
            unsafe { IrqGuardToken::from_raw(1) }
        }
        unsafe fn irq_guard_exit(_token: IrqGuardToken) {}

        fn preempt_guard_enter() -> PreemptGuardToken {
            PREEMPT_GUARD_ENTRIES.with(|entries| entries.set(entries.get() + 1));
            PREEMPT_DEPTH.with(|depth| {
                depth.set(depth.get().checked_add(1).expect("test preempt depth overflow"));
            });
            // SAFETY: the test runtime models a balanced scalar preemption
            // token with PREEMPT_DEPTH.
            unsafe { PreemptGuardToken::from_raw(1) }
        }

        unsafe fn preempt_guard_exit(_token: PreemptGuardToken) {
            PREEMPT_DEPTH.with(|depth| {
                depth.set(
                    depth
                        .get()
                        .checked_sub(1)
                        .expect("test preempt exit without enter"),
                );
            });
        }

        fn publish_local_scheduler_work() -> bool {
            false
        }
        fn finish_context_switch_tail() {}
        fn finish_initial_context_switch() {}
        fn scheduler_frame_guard_enter(
            _origin: RuntimeScheduleOrigin,
            _entry: RuntimeSchedulerEntry,
        ) -> RuntimeStatus { RuntimeStatus::Success }
        fn scheduler_frame_guard_exit(_return_to: RuntimeSchedulerReturn) -> bool { true }
        fn in_hard_irq() -> bool { false }
        fn validate_schedule_context(_origin: ax_task::runtime::RuntimeScheduleOrigin) -> RuntimeStatus {
            if SCHEDULE_CONTEXT_SAFE.with(Cell::get) {
                RuntimeStatus::Success
            } else {
                RuntimeStatus::UnsafeContext
            }
        }
        fn validate_owner_cpu_context() -> RuntimeStatus { RuntimeStatus::Success }
        fn monotonic_now() -> ax_task::runtime::MonotonicInstant {
            ax_task::runtime::MonotonicInstant::from_nanos(0).unwrap()
        }
        fn rq_clock_sample(_cpu: RuntimeCpuId) -> RqClockSample {
            RqClockSample::new(ax_task::SchedulerTimestamp::from_nanos(0), 0)
        }
        fn publish_scheduler_deadline(_update: SchedulerDeadlineUpdate) {}
        fn notify_scheduler_cpu(_cpu: RuntimeCpuId) -> RuntimeStatus {
            RuntimeStatus::Success
        }
        fn wait_for_interrupt() {}
        fn allocate_stack(_request: StackRequest) -> RuntimeHandleResult {
            RuntimeHandleResult::failure(RuntimeStatus::Unsupported)
        }
        fn deallocate_stack(_stack: StackHandle) {}
        fn allocate_tls(_request: TlsRequest) -> RuntimeHandleResult {
            RuntimeHandleResult::failure(RuntimeStatus::Unsupported)
        }
        fn deallocate_tls(_tls: TlsHandle) {}
        fn create_kernel_context(_request: KernelContextRequest) -> RuntimeHandleResult {
            RuntimeHandleResult::failure(RuntimeStatus::Unsupported)
        }
        fn create_user_context(_request: UserContextRequest) -> RuntimeHandleResult {
            RuntimeHandleResult::failure(RuntimeStatus::Unsupported)
        }
        fn bind_context_thread(_binding: ContextThreadBinding) -> RuntimeStatus {
            RuntimeStatus::Success
        }
        fn destroy_context(_context: ExecutionContextHandle) {}
        fn destroy_address_space(
            _address_space: AddressSpaceHandle,
        ) -> AddressSpaceDestroyOutcome {
            panic!("ax-sync unit tests do not own address-space tokens")
        }
        fn arm_address_space_reclaim(
            _address_space: AddressSpaceHandle,
        ) -> AddressSpaceReclaimArmOutcome {
            panic!("ax-sync unit tests do not own address-space tokens")
        }
        fn address_space_membarrier_state(
            _address_space: AddressSpaceHandle,
        ) -> AddressSpaceMembarrierState {
            panic!("ax-sync unit tests do not own address-space tokens")
        }
        fn update_address_space_membarrier_state(
            _address_space: AddressSpaceHandle,
            _registration: MembarrierRegistration,
            _phase: MembarrierRegistrationPhase,
        ) -> AddressSpaceMembarrierState {
            panic!("ax-sync unit tests do not own address-space tokens")
        }
        fn synchronize_membarrier_cpu(
            _cpu: RuntimeCpuId,
            action: RuntimeMembarrierAction,
        ) -> RuntimeStatus {
            if action == RuntimeMembarrierAction::MemoryBarrier {
                core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
            }
            RuntimeStatus::Success
        }
        unsafe fn switch_context(_switch: ContextSwitch) {
            panic!("unit-test runtime has no execution contexts")
        }
        fn activate_address_space(_activation: AddressSpaceActivation) -> RuntimeStatus {
            RuntimeStatus::Unsupported
        }
        fn flush_tlb_local(_start: usize, _size: usize) {}
        fn trace_sched_switch(_record: SchedSwitchRecord) {}
        fn fatal_invariant(_code: u32, _argument: usize) -> ! {
            panic!("scheduler invariant reported by ax-sync unit test")
        }
    }
}

#[test]
fn pure_model_exports_the_context_binding_symbol() {
    assert_eq!(
        ax_task::runtime::task_runtime::bind_context_thread(ContextThreadBinding {
            context: ExecutionContextHandle::NONE,
            publication: CurrentThreadPublication::NONE,
        }),
        RuntimeStatus::Success
    );
}

#[cfg(feature = "lockdep")]
struct UnitTestKspinLockdep;

#[cfg(feature = "lockdep")]
#[ax_crate_interface::impl_interface]
impl ax_lockdep::KspinLockdepIf for UnitTestKspinLockdep {
    fn collect_current_task_held_locks(snapshot: &mut ax_lockdep::HeldLockSnapshot) {
        HELD_LOCKS.with(|held| snapshot.extend(&held.borrow()));
    }

    fn push_current_task_held_lock(held: ax_lockdep::HeldLock) {
        HELD_LOCKS.with(|stack| stack.borrow_mut().push(held));
    }

    fn pop_current_task_held_lock(lock_addr: usize) {
        HELD_LOCKS.with(|stack| stack.borrow_mut().pop_checked(lock_addr));
    }

    fn console_write_str(_text: &str) {}

    fn fatal() -> ! {
        panic!("ax-sync unit-test lockdep fatal")
    }
}

pub(crate) struct InstalledRuntime;

impl Drop for InstalledRuntime {
    fn drop(&mut self) {
        clear();
    }
}

pub(crate) fn install(task_system: usize, cpu_local: usize) -> InstalledRuntime {
    let publication = if task_system == 0 {
        CurrentThreadPublication::NONE
    } else {
        // SAFETY: the caller retains this TaskSystem until `clear`.
        let system = unsafe { &*core::ptr::with_exposed_provenance::<TaskSystem>(task_system) };
        system
            .cpu_remote(CpuId::new(0))
            .and_then(CpuRemote::current_thread)
            .and_then(|thread| system.thread_handle(thread).ok())
            .map_or(CurrentThreadPublication::NONE, |thread| {
                thread.runtime_publication()
            })
    };
    TASK_SYSTEM.with(|handle| handle.set(task_system));
    CPU_LOCAL.with(|handle| handle.set(cpu_local));
    CURRENT_PUBLICATION.with(|current| current.set(publication));
    let remote = if task_system == 0 {
        0
    } else {
        // SAFETY: the caller retains this TaskSystem until clear.
        let system = unsafe { &*core::ptr::with_exposed_provenance::<TaskSystem>(task_system) };
        system.runtime_cpu_remote_handle(CpuId::new(0)).into_raw()
    };
    CPU_REMOTE.with(|handle| handle.set(remote));
    SCHEDULE_CONTEXT_SAFE.with(|safe| safe.set(true));
    PREEMPT_DEPTH.with(|depth| depth.set(0));
    IRQ_GUARD_ENTRIES.with(|entries| entries.set(0));
    CPU_OWNER_HANDLE_READS.with(|reads| reads.set(0));
    PREEMPT_GUARD_ENTRIES.with(|entries| entries.set(0));
    InstalledRuntime
}

pub(crate) fn clear() {
    CURRENT_PUBLICATION.with(|current| current.set(CurrentThreadPublication::NONE));
    CPU_REMOTE.with(|handle| handle.set(0));
    CPU_LOCAL.with(|handle| handle.set(0));
    TASK_SYSTEM.with(|handle| handle.set(0));
    PREEMPT_DEPTH.with(|depth| depth.set(0));
    IRQ_GUARD_ENTRIES.with(|entries| entries.set(0));
    CPU_OWNER_HANDLE_READS.with(|reads| reads.set(0));
    PREEMPT_GUARD_ENTRIES.with(|entries| entries.set(0));
    SCHEDULE_CONTEXT_SAFE.with(|safe| safe.set(true));
}

pub(crate) fn set_schedule_context_safe(safe: bool) {
    SCHEDULE_CONTEXT_SAFE.with(|state| state.set(safe));
}

pub(crate) fn preempt_depth() -> usize {
    PREEMPT_DEPTH.with(Cell::get)
}

pub(crate) fn irq_guard_entries() -> usize {
    IRQ_GUARD_ENTRIES.with(Cell::get)
}

pub(crate) fn reset_cpu_owner_handle_reads() {
    CPU_OWNER_HANDLE_READS.with(|reads| reads.set(0));
}

pub(crate) fn cpu_owner_handle_reads() -> usize {
    CPU_OWNER_HANDLE_READS.with(Cell::get)
}

pub(crate) fn reset_preempt_guard_entries() {
    PREEMPT_GUARD_ENTRIES.with(|entries| entries.set(0));
}

pub(crate) fn preempt_guard_entries() -> usize {
    PREEMPT_GUARD_ENTRIES.with(Cell::get)
}
