//! VM interrupt delivery models and deferred runtime wake plumbing.

pub(crate) mod deferred;
pub(crate) mod model;
pub(crate) mod sender;

#[cfg(target_arch = "aarch64")]
pub use ax_plat::irq::GuestIrqInjection;

/// Host platform hook for forwarding current-EL AArch64 GIC interrupts into
/// the guest that owns the physical line.
#[cfg(target_arch = "aarch64")]
#[ax_crate_interface::def_interface]
pub trait Aarch64PlatformIrqInjectorIf {
    fn register_virtual_irq_injector(injector: fn(usize, u8) -> GuestIrqInjection);
}

#[cfg(target_arch = "aarch64")]
pub(crate) fn register_aarch64_virtual_irq_injector(injector: fn(usize, u8) -> GuestIrqInjection) {
    ax_crate_interface::call_interface!(
        Aarch64PlatformIrqInjectorIf::register_virtual_irq_injector(injector)
    );
}
