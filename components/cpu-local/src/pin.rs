use core::{marker::PhantomData, ptr::NonNull};

use crate::{CpuAreaRef, CpuLocalError, register};

/// Scoped proof that execution cannot migrate away from one validated CPU.
///
/// The token can only be created by [`with_cpu_pin`]. Its invariant lifetime
/// and higher-ranked callback prevent it from escaping the caller's migration
/// guard or offline-CPU critical section.
#[must_use = "CPU-local access is valid only while this pin remains in scope"]
#[derive(Debug)]
pub struct CpuPin<'scope> {
    area: CpuAreaRef,
    _scope: PhantomData<&'scope mut &'scope ()>,
    _not_send_or_sync: PhantomData<*mut ()>,
}

impl CpuPin<'_> {
    /// Returns the initialized CPU area validated when this pin was created.
    pub const fn area(&self) -> CpuAreaRef {
        self.area
    }
}

/// Scoped proof of exclusive local access to CPU-owned mutable state.
///
/// In addition to migration exclusion, the caller that creates this token has
/// excluded local IRQ/re-entry and every conflicting remote access.
#[must_use = "mutable CPU-local access is valid only while this token remains in scope"]
#[derive(Debug)]
pub struct ExclusiveCpu<'pin> {
    area: CpuAreaRef,
    _scope: PhantomData<&'pin mut &'pin ()>,
    _not_send_or_sync: PhantomData<*mut ()>,
}

/// Non-escaping selection of the CPU area owned by a scheduler/IRQ boundary.
///
/// This is intentionally weaker than [`CpuPin`]: it selects CPU-owned state
/// without validating current-task publication. Only low-level scheduler,
/// interrupt, and offline-bootstrap code should receive this capability.
#[doc(hidden)]
#[must_use = "scheduler CPU-area access is valid only while this token remains in scope"]
pub struct SchedulerCpuArea<'scope> {
    area_base: usize,
    _scope: PhantomData<&'scope mut &'scope ()>,
    _not_send_or_sync: PhantomData<*mut ()>,
}

impl SchedulerCpuArea<'_> {
    /// Calculates a typed symbol address in the selected installed CPU area.
    ///
    /// Constructing the pointer does not grant permission to dereference it;
    /// the symbol provider and outer owner transaction retain that contract.
    #[doc(hidden)]
    pub fn symbol_ptr<T>(&self, offset: usize) -> Result<NonNull<T>, CpuLocalError> {
        let address = self
            .area_base
            .checked_add(offset)
            .ok_or(CpuLocalError::AddressOverflow)?;
        NonNull::new(address as *mut T).ok_or(CpuLocalError::InvalidAreaBase { base: address })
    }
}

impl ExclusiveCpu<'_> {
    /// Returns the initialized area covered by this stronger capability.
    pub const fn area(&self) -> CpuAreaRef {
        self.area
    }
}

/// Runs `operation` with a validated, non-escaping CPU pin.
///
/// The higher-ranked callback prevents retaining the token:
///
/// ```compile_fail
/// let retained = unsafe { cpu_local::with_cpu_pin(|pin| pin) }.unwrap();
/// # let _ = retained;
/// ```
///
/// It also cannot be sent to another execution context:
///
/// ```compile_fail
/// unsafe {
///     cpu_local::with_cpu_pin(|pin| {
///         std::thread::scope(|scope| scope.spawn(|| drop(pin)));
///     })
///     .unwrap();
/// }
/// ```
///
/// # Errors
///
/// Returns [`CpuLocalError::AreaNotInstalled`] before this CPU has installed
/// its runtime area, or an identity error if the live register and area header
/// disagree.
///
/// # Safety
///
/// The caller must prevent migration for the complete callback. Offline boot
/// code may call this while the CPU cannot be scheduled; runtime code must
/// hold an appropriate preemption or IRQ guard.
pub unsafe fn with_cpu_pin<R>(
    operation: impl for<'scope> FnOnce(&CpuPin<'scope>) -> R,
) -> Result<R, CpuLocalError> {
    let area = register::current_area()?;
    let pin = CpuPin {
        area,
        _scope: PhantomData,
        _not_send_or_sync: PhantomData,
    };
    // Validate the second architecture-owned source before exposing any
    // typed access. This catches a restored CPU base paired with a stale task
    // register (notably after a vCPU exit) at the pin boundary.
    register::current_thread(&pin)?;
    Ok(operation(&pin))
}

/// Runs `operation` with exclusive access to mutable state on the pinned CPU.
///
/// # Safety
///
/// The caller must prevent migration, local IRQ/re-entry, and conflicting
/// remote access for the complete callback. `pin` must be covered by the same
/// guard that establishes those conditions.
pub unsafe fn with_exclusive_cpu<R>(
    pin: &CpuPin<'_>,
    operation: impl for<'exclusive> FnOnce(&ExclusiveCpu<'exclusive>) -> R,
) -> R {
    let exclusive = ExclusiveCpu {
        area: pin.area,
        _scope: PhantomData,
        _not_send_or_sync: PhantomData,
    };
    operation(&exclusive)
}

/// Runs `operation` with a scheduler-owned CPU-area selection.
///
/// Unlike [`with_cpu_pin`], this boundary does not route CPU-owned state
/// through the current task and does not repeat full area identity validation.
/// The higher-ranked callback prevents the selection token from escaping.
///
/// # Safety
///
/// The caller must prevent migration and context switches for the complete
/// callback. Mutable values selected through this token additionally require
/// local IRQ/re-entry and every conflicting remote access to be excluded.
/// Offline bootstrap satisfies these conditions before interrupt publication.
#[doc(hidden)]
pub unsafe fn with_scheduler_cpu_area<R>(
    operation: impl for<'scope> FnOnce(&SchedulerCpuArea<'scope>) -> R,
) -> Result<R, CpuLocalError> {
    let area_base = unsafe { register::scheduler_current_cpu_base()? };
    let area = SchedulerCpuArea {
        area_base,
        _scope: PhantomData,
        _not_send_or_sync: PhantomData,
    };
    Ok(operation(&area))
}
