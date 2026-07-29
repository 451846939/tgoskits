//! AArch64 GIC host operations for the ArceOS-backed AxVM runtime.

use core::sync::atomic::{AtomicUsize, Ordering};

use arm_gic_driver::v3::{
    ICH_ELRSR_EL2, ICH_HCR_EL2, ICH_LR_EL2, ICH_VTR_EL2, ReadWriteable, Readable, Writeable,
    ich_lr_el2_get, ich_lr_el2_write,
};
use ax_memory_addr::{PhysAddr, VirtAddr};
use spin::Once;

use super::GUEST_VIRTUAL_TIMER_IRQ;
use crate::host::{HostMemory, default_host};

static GUEST_VIRTUAL_TIMER_IRQ_ID: Once<ax_std::os::arceos::modules::ax_hal::irq::IrqId> =
    Once::new();
static GUEST_VIRTUAL_TIMER_IRQ_REGISTERED_CPUS: AtomicUsize = AtomicUsize::new(0);

fn with_gic<T>(f: impl FnOnce(&mut rdif_intc::Intc) -> T) -> T {
    let mut gic = rdrive::get_one::<rdif_intc::Intc>()
        .expect("failed to get GIC driver")
        .lock()
        .expect("failed to lock GIC driver");
    f(&mut gic)
}

pub(crate) fn reset_virtual_interface() {
    ICH_HCR_EL2.set(0);
    let lr_num = ICH_VTR_EL2.read(ICH_VTR_EL2::LISTREGS) as usize + 1;
    for index in 0..lr_num {
        ich_lr_el2_write(index, ICH_LR_EL2::STATE::Invalid);
    }
    ICH_HCR_EL2.modify(ICH_HCR_EL2::EN::SET);
}

fn guest_virtual_timer_irq_id() -> ax_std::os::arceos::modules::ax_hal::irq::IrqId {
    use ax_std::os::arceos::modules::ax_hal::irq::{HwIrq, resolve_percpu_irq};

    *GUEST_VIRTUAL_TIMER_IRQ_ID.call_once(|| {
        resolve_percpu_irq(HwIrq(GUEST_VIRTUAL_TIMER_IRQ as u32))
            .expect("failed to resolve the AArch64 guest virtual timer PPI")
    })
}

pub(crate) fn register_guest_virtual_timer_irq_on_current_cpu() {
    use ax_std::os::arceos::modules::ax_hal::irq::{
        self, CpuId, CpuMask, IrqRequest, IrqScope, ShareMode,
    };

    let _guard = ax_kernel_guard::NoPreempt::new();
    let cpu_id = ax_std::os::arceos::modules::ax_hal::percpu::this_cpu_id();
    let cpu_bit = 1usize
        .checked_shl(cpu_id as u32)
        .expect("AArch64 guest timer PPI CPU id exceeds the registration bitmap");
    if GUEST_VIRTUAL_TIMER_IRQ_REGISTERED_CPUS.fetch_or(cpu_bit, Ordering::AcqRel) & cpu_bit != 0 {
        return;
    }

    let cpus = CpuMask::from_cpu(CpuId(cpu_id));
    let request = IrqRequest::new_concurrent(guest_virtual_timer_irq_handler)
        .scope(IrqScope::PerCpu { cpus })
        .share_mode(ShareMode::Shared);
    if let Err(err) = irq::request_irq(guest_virtual_timer_irq_id(), request) {
        GUEST_VIRTUAL_TIMER_IRQ_REGISTERED_CPUS.fetch_and(!cpu_bit, Ordering::AcqRel);
        panic!("failed to register the AArch64 guest virtual timer PPI on CPU {cpu_id}: {err:?}");
    }
}

fn guest_virtual_timer_irq_handler(
    ctx: ax_std::os::arceos::modules::ax_hal::irq::IrqContext,
) -> ax_std::os::arceos::modules::ax_hal::irq::IrqReturn {
    use ax_std::os::arceos::modules::ax_hal::irq::{IrqReturn, set_enable};

    if let Err(err) = set_enable(ctx.irq, false) {
        warn!(
            "failed to mask AArch64 guest virtual timer PPI {:?} on CPU {}: {err:?}",
            ctx.irq, ctx.cpu.0
        );
        return IrqReturn::Unhandled;
    }
    if let Err(err) = crate::manager::inject_current_vcpu_interrupt(GUEST_VIRTUAL_TIMER_IRQ) {
        warn!(
            "failed to inject AArch64 guest virtual timer PPI on CPU {}: {err:?}",
            ctx.cpu.0
        );
    }

    IrqReturn::Handled
}

pub(crate) fn mask_guest_virtual_timer_irq() {
    use ax_std::os::arceos::modules::ax_hal::irq::set_enable;

    if let Err(err) = set_enable(guest_virtual_timer_irq_id(), false) {
        warn!("failed to mask the AArch64 guest virtual timer PPI after VM exit: {err:?}");
    }
}

pub(crate) fn rearm_guest_virtual_timer_irq_if_inactive() {
    use ax_std::os::arceos::modules::ax_hal::irq::set_enable;

    if guest_virtual_timer_signal_asserted() {
        if let Err(err) = crate::manager::inject_current_vcpu_interrupt(GUEST_VIRTUAL_TIMER_IRQ) {
            warn!("failed to reinject the asserted AArch64 guest virtual timer PPI: {err:?}");
        }
    } else if let Err(err) = set_enable(guest_virtual_timer_irq_id(), true) {
        warn!("failed to rearm the AArch64 guest virtual timer PPI: {err:?}");
    }
}

fn guest_virtual_timer_signal_asserted() -> bool {
    const ENABLE: u64 = 1 << 0;
    const IMASK: u64 = 1 << 1;
    const ISTATUS: u64 = 1 << 2;

    let cntv_ctl: u64;
    // SAFETY: CNTV_CTL_EL0 is an architected per-CPU timer status register.
    // Reading it has no memory, interrupt-controller, or timer state side
    // effects, and this function runs while the guest vCPU is CPU-pinned.
    unsafe {
        core::arch::asm!("mrs {value}, CNTV_CTL_EL0", value = out(reg) cntv_ctl);
    }
    cntv_ctl & ENABLE != 0 && cntv_ctl & IMASK == 0 && cntv_ctl & ISTATUS != 0
}

pub(crate) fn inject_interrupt(irq: usize) {
    debug!("Injecting virtual interrupt: {irq}");

    with_gic(|gic| {
        if let Some(gic) = gic.typed_mut::<arm_gic_driver::v2::Gic>() {
            use arm_gic_driver::{
                IntId,
                v2::{VirtualInterruptConfig, VirtualInterruptState},
            };

            let gich = gic.hypervisor_interface().expect("failed to get GICH");
            gich.enable();
            gich.set_virtual_interrupt(
                0,
                VirtualInterruptConfig::software(
                    unsafe { IntId::raw(irq as _) },
                    None,
                    0,
                    VirtualInterruptState::Pending,
                    false,
                    true,
                ),
            );
            return;
        }

        if gic.typed_mut::<arm_gic_driver::v3::Gic>().is_some() {
            inject_interrupt_gic_v3(irq);
            return;
        }

        panic!("no GIC driver found");
    });
}

fn inject_interrupt_gic_v3(vector: usize) {
    debug!("Injecting virtual interrupt: vector={vector}");
    let elsr = ICH_ELRSR_EL2.read(ICH_ELRSR_EL2::STATUS);
    let lr_num = ICH_VTR_EL2.read(ICH_VTR_EL2::LISTREGS) as usize + 1;

    let mut free_lr = None;
    for i in 0..lr_num {
        if (1 << i) & elsr > 0 {
            free_lr.get_or_insert(i);
            continue;
        }

        let lr_val = ich_lr_el2_get(i);
        if lr_val.read(ICH_LR_EL2::VINTID) == vector as u64
            && lr_val.matches_any(&[ICH_LR_EL2::STATE::Pending, ICH_LR_EL2::STATE::Active])
        {
            debug!("Virtual interrupt {vector} already pending/active in LR{i}, skipping");
            return;
        }
    }

    let free_lr = free_lr
        .or_else(|| {
            (0..lr_num).find(|&i| ich_lr_el2_get(i).matches_all(ICH_LR_EL2::STATE::Invalid))
        })
        .unwrap_or_else(|| panic!("no free list register to inject IRQ {vector}"));

    ich_lr_el2_write(
        free_lr,
        ICH_LR_EL2::VINTID.val(vector as u64) + ICH_LR_EL2::STATE::Pending + ICH_LR_EL2::GROUP::SET,
    );
    if !ICH_HCR_EL2.is_set(ICH_HCR_EL2::EN) {
        warn!("Virtual interrupt interface not enabled, enabling now");
        ICH_HCR_EL2.modify(ICH_HCR_EL2::EN::SET);
    }

    debug!("Virtual interrupt {vector} injected successfully in LR{free_lr}");
}

pub(crate) fn read_gicd_iidr() -> u32 {
    with_gic(|gic| {
        if let Some(gic) = gic.typed_mut::<arm_gic_driver::v2::Gic>() {
            return gic.iidr_raw();
        }
        if let Some(gic) = gic.typed_mut::<arm_gic_driver::v3::Gic>() {
            return gic.iidr_raw();
        }
        panic!("no GIC driver found");
    })
}

pub(crate) fn read_gicd_typer() -> u32 {
    with_gic(|gic| {
        if let Some(gic) = gic.typed_mut::<arm_gic_driver::v2::Gic>() {
            return gic.typer_raw();
        }
        if let Some(gic) = gic.typed_mut::<arm_gic_driver::v3::Gic>() {
            return gic.typer_raw();
        }
        panic!("no GIC driver found");
    })
}

pub(crate) fn host_gicd_base() -> PhysAddr {
    with_gic(|gic| {
        if let Some(gic) = gic.typed_mut::<arm_gic_driver::v2::Gic>() {
            return default_host().virt_to_phys(VirtAddr::from(usize::from(gic.gicd_addr())));
        }
        if let Some(gic) = gic.typed_mut::<arm_gic_driver::v3::Gic>() {
            return default_host().virt_to_phys(VirtAddr::from(usize::from(gic.gicd_addr())));
        }
        panic!("no GIC driver found");
    })
}

pub(crate) fn host_gicr_base() -> PhysAddr {
    with_gic(|gic| {
        if let Some(gic) = gic.typed_mut::<arm_gic_driver::v3::Gic>() {
            return default_host().virt_to_phys(VirtAddr::from(usize::from(gic.gicr_addr())));
        }
        panic!("no GICv3 driver found");
    })
}

pub(crate) fn handle_current_irq() -> Option<usize> {
    // AArch64 ArceOS platform IRQ handlers acknowledge the current IRQ
    // internally. The raw vector argument is ignored by current GIC-backed
    // platforms, so keep the ack/EOI ownership inside the platform handler
    // while preserving the GIC INTID for guest-owned virtual sources.
    ax_std::os::arceos::modules::ax_hal::irq::handle_irq_id(0).map(|irq| irq.hwirq.0 as usize)
}

pub(crate) fn fetch_irq() -> Option<usize> {
    handle_current_irq()
}
