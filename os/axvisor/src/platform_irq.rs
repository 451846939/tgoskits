// Copyright 2026 The TGOSKits Authors
// SPDX-License-Identifier: Apache-2.0

struct Aarch64PlatformIrqInjector;

#[ax_crate_interface::impl_interface]
impl axvm::irq::Aarch64PlatformIrqInjectorIf for Aarch64PlatformIrqInjector {
    fn register_virtual_irq_injector(injector: fn(usize, u8) -> axvm::irq::GuestIrqInjection) {
        axplat_dyn::register_aarch64_virtual_irq_injector(injector);
    }
}
