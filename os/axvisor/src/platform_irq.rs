#[cfg(target_arch = "aarch64")]
struct Aarch64PlatformIrqInjector;

#[cfg(target_arch = "aarch64")]
#[ax_crate_interface::impl_interface]
impl axvm::irq::Aarch64PlatformIrqInjectorIf for Aarch64PlatformIrqInjector {
    fn register_virtual_irq_injector(injector: fn(usize, u8) -> axvm::irq::GuestIrqInjection) {
        axplat_dyn::register_virtual_irq_injector(injector);
    }
}

#[cfg(target_arch = "riscv64")]
struct RiscvPlatformIrqInjector;

#[cfg(target_arch = "riscv64")]
#[ax_crate_interface::impl_interface]
impl axvm::irq::RiscvPlatformIrqInjectorIf for RiscvPlatformIrqInjector {
    fn register_virtual_irq_injector(injector: fn(usize) -> bool) {
        axplat_dyn::register_virtual_irq_injector(injector);
    }
}
