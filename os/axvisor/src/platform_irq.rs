struct RiscvPlatformIrqInjector;

#[ax_crate_interface::impl_interface]
impl axvm::irq::RiscvPlatformIrqInjectorIf for RiscvPlatformIrqInjector {
    fn register_virtual_irq_injector(injector: fn(usize, usize, usize) -> bool) {
        axplat_dyn::register_virtual_irq_injector(injector);
    }

    fn set_virtual_irq_targets(vm_id: usize, vcpu_id: usize, cpu_id: usize, irq_sources: &[u32]) {
        axplat_dyn::set_virtual_irq_targets(vm_id, vcpu_id, cpu_id, irq_sources);
    }

    fn clear_virtual_irq_targets(vm_id: usize) {
        axplat_dyn::clear_virtual_irq_targets(vm_id);
    }
}
