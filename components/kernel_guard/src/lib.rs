//! RAII wrappers to create a critical section with local IRQs or preemption
//! disabled, used to implement spin locks in kernel.
//!
//! The critical section is created after the guard struct is created, and is
//! ended when the guard falls out of scope.
//!
//! The crate user must implement the [`KernelGuardIf`] trait using
//! [`ax_crate_interface::impl_interface`] to provide the low-level implementantion
//! of how to enable/disable kernel preemption, if the feature `preempt` is
//! enabled.
//!
//! Available guards:
//!
//! - [`NoOp`]: Does nothing around the critical section.
//! - [`IrqSave`]: Disables/enables local IRQs around the critical section.
//! - [`NoPreempt`]: Disables/enables kernel preemption around the critical
//!   section.
//! - [`NoPreemptIrqSave`]: Disables/enables both kernel preemption and local
//!   IRQs around the critical section.
//!
//! # Crate features
//!
//! - `preempt`: Use in the preemptive system. If this feature is enabled, you
//!   need to implement the [`KernelGuardIf`] trait in other crates. Otherwise
//!   the preemption enable/disable operations will be no-ops. This feature is
//!   disabled by default.
//! - `host-test`: Avoid privileged IRQ instructions for host unit tests. This
//!   feature is disabled by default.
//!
//! # Examples
//!
//! ```
//! use ax_kernel_guard::{KernelGuardIf, NoPreempt};
//!
//! struct KernelGuardIfImpl;
//!
//! #[ax_crate_interface::impl_interface]
//! impl KernelGuardIf for KernelGuardIfImpl {
//!     fn enable_preempt() {
//!         // Your implementation here
//!     }
//!     fn enable_preempt_from_irq_return() {
//!         // Your IRQ-return implementation here
//!     }
//!     fn disable_preempt() {
//!         // Your implementation here
//!     }
//! }
//!
//! let guard = NoPreempt::new();
//! // The critical section starts here
//! //
//! // Do something that requires preemption to be disabled
//! //
//! // The critical section ends here
//! drop(guard);
//! ```

#![no_std]

#[cfg(all(axtest, feature = "axtest"))]
/// Coverage tests for kernel guard state transitions.
pub mod axtest;

mod arch;

/// Low-level interfaces that must be implemented by the crate user.
#[ax_crate_interface::def_interface]
pub trait KernelGuardIf {
    /// Enables kernel preemption from ordinary task context.
    ///
    /// If local IRQs are disabled, this path must defer scheduling. It must not
    /// infer an IRQ-return boundary from the live hardware IRQ state.
    fn enable_preempt();

    /// Enables kernel preemption at an explicit IRQ-return boundary.
    ///
    /// The implementation may enter the scheduler while local IRQs remain
    /// disabled, but must return with them still disabled for the architecture
    /// exception epilogue.
    fn enable_preempt_from_irq_return();

    /// How to disable kernel preemption.
    fn disable_preempt();
}

/// A base trait that all guards implement.
pub trait BaseGuard {
    /// The saved state when entering the critical section.
    type State: Clone + Copy;

    /// Something that must be done before entering the critical section.
    fn acquire() -> Self::State;

    /// Something that must be done after leaving the critical section.
    fn release(state: Self::State);

    /// Returns whether locks guarded by this type should participate in
    /// lock dependency tracking.
    fn lockdep_enabled() -> bool {
        false
    }
}

/// A no-op guard that does nothing around the critical section.
pub struct NoOp;

/// A guard that disables/enables local IRQs around the critical section.
pub struct IrqSave(usize);

/// A preemption guard whose final release is an explicit IRQ-return boundary.
///
/// The mutable borrow ties this guard to a live [`IrqSave`], so the architecture
/// IRQ state cannot be restored before the preemption exit has completed.
#[must_use = "dropping the guard completes the IRQ-return preemption exit"]
pub struct IrqReturnPreemptGuard<'irq> {
    _irq_guard: core::marker::PhantomData<&'irq mut IrqSave>,
}

/// A guard that disables/enables kernel preemption around the critical section.
pub struct NoPreempt;

/// A guard that disables/enables both kernel preemption and local IRQs around
/// the critical section.
///
/// When entering the critical section, it disables kernel preemption first,
/// followed by local IRQs. When leaving the critical section, it re-enables
/// local IRQs first, followed by kernel preemption.
pub struct NoPreemptIrqSave(usize);

impl BaseGuard for NoOp {
    type State = ();
    fn acquire() -> Self::State {}
    fn release(_state: Self::State) {}
}

impl NoOp {
    /// Creates a new [`NoOp`] guard.
    pub const fn new() -> Self {
        Self
    }
}

impl Default for NoOp {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for NoOp {
    fn drop(&mut self) {}
}

mod imp {
    use super::*;

    impl BaseGuard for IrqSave {
        type State = usize;

        #[inline]
        fn acquire() -> Self::State {
            super::arch::local_irq_save_and_disable()
        }

        #[inline]
        fn release(state: Self::State) {
            // restore IRQ states
            super::arch::local_irq_restore(state);
        }

        fn lockdep_enabled() -> bool {
            // Keep this disabled for now. The current task-only lockdep model
            // no longer depends on per-CPU held-lock state, but the codebase
            // does not currently expose any BaseSpinLock<IrqSave, _> aliases or
            // real users, so there is no need to widen the tracked guard set
            // until that use case is defined and tested.
            false
        }
    }

    impl BaseGuard for NoPreempt {
        type State = ();
        fn acquire() -> Self::State {
            // disable preempt
            #[cfg(feature = "preempt")]
            ax_crate_interface::call_interface!(KernelGuardIf::disable_preempt);
        }
        fn release(_state: Self::State) {
            // enable preempt
            #[cfg(feature = "preempt")]
            ax_crate_interface::call_interface!(KernelGuardIf::enable_preempt);
        }

        fn lockdep_enabled() -> bool {
            true
        }
    }

    impl BaseGuard for NoPreemptIrqSave {
        type State = usize;
        fn acquire() -> Self::State {
            // disable preempt
            #[cfg(feature = "preempt")]
            ax_crate_interface::call_interface!(KernelGuardIf::disable_preempt);
            // disable IRQs and save IRQ states
            super::arch::local_irq_save_and_disable()
        }
        fn release(state: Self::State) {
            // restore IRQ states
            super::arch::local_irq_restore(state);
            // enable preempt
            #[cfg(feature = "preempt")]
            ax_crate_interface::call_interface!(KernelGuardIf::enable_preempt);
        }

        fn lockdep_enabled() -> bool {
            true
        }
    }

    impl IrqSave {
        /// Creates a new [`IrqSave`] guard.
        pub fn new() -> Self {
            Self(Self::acquire())
        }

        /// Disables preemption for work completed by an IRQ-return epilogue.
        ///
        /// Unlike [`NoPreempt`], dropping the returned guard explicitly permits
        /// IRQ-return scheduling while hardware IRQs remain disabled. The
        /// borrow prevents this [`IrqSave`] from being released first.
        pub fn disable_preempt_for_irq_return(&mut self) -> IrqReturnPreemptGuard<'_> {
            NoPreempt::acquire();
            IrqReturnPreemptGuard {
                _irq_guard: core::marker::PhantomData,
            }
        }
    }

    impl Drop for IrqReturnPreemptGuard<'_> {
        fn drop(&mut self) {
            #[cfg(feature = "preempt")]
            ax_crate_interface::call_interface!(KernelGuardIf::enable_preempt_from_irq_return);
        }
    }

    impl Drop for IrqSave {
        fn drop(&mut self) {
            Self::release(self.0)
        }
    }

    impl Default for IrqSave {
        fn default() -> Self {
            Self::new()
        }
    }

    impl NoPreempt {
        /// Creates a new [`NoPreempt`] guard.
        pub fn new() -> Self {
            Self::acquire();
            Self
        }
    }

    impl Drop for NoPreempt {
        fn drop(&mut self) {
            Self::release(())
        }
    }

    impl Default for NoPreempt {
        fn default() -> Self {
            Self::new()
        }
    }

    impl NoPreemptIrqSave {
        /// Creates a new [`NoPreemptIrqSave`] guard.
        pub fn new() -> Self {
            Self(Self::acquire())
        }
    }

    impl Drop for NoPreemptIrqSave {
        fn drop(&mut self) {
            Self::release(self.0)
        }
    }

    impl Default for NoPreemptIrqSave {
        fn default() -> Self {
            Self::new()
        }
    }
}
