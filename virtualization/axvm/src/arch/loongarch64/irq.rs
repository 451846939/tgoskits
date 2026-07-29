//! LoongArch platform IRQ routing used by AxVM.

use alloc::sync::Arc;

use axdevice_base::{InterruptTriggerMode, IrqError, IrqLineId, IrqResult, IrqSink};
use axvm_types::VMInterruptMode;

const EIOINTC_IRQ: usize = 3;

struct LoongArchPchPicIrqSink {
    vm_id: usize,
}

impl IrqSink for LoongArchPchPicIrqSink {
    fn set_level(&self, line: IrqLineId, asserted: bool) -> IrqResult {
        let vm = crate::get_vm_by_id(self.vm_id).ok_or_else(|| IrqError::Backend {
            line,
            operation: "route LoongArch virtual IRQ",
            detail: alloc::format!("VM[{}] is not registered", self.vm_id),
        })?;
        let devices = vm.get_devices().map_err(|error| IrqError::Backend {
            line,
            operation: "route LoongArch virtual IRQ",
            detail: alloc::format!("{error}"),
        })?;
        let Some(vector) = devices.loongarch_pch_pic_set_irq_level(line.0, asserted) else {
            return Err(IrqError::Unsupported {
                line,
                operation: "route LoongArch virtual IRQ",
                detail: "the VM has no virtual PCH-PIC".into(),
            });
        };
        if !asserted {
            return Ok(());
        }
        let Some(vector) = vector else {
            return Ok(());
        };
        crate::irq::dispatch_runtime_interrupt(
            self.vm_id,
            0,
            line,
            vector,
            InterruptTriggerMode::LevelTriggered,
        )
    }

    fn pulse(&self, line: IrqLineId) -> IrqResult {
        self.set_level(line, true)?;
        self.set_level(line, false)
    }
}

pub(crate) fn interrupt_fabric(
    vm_id: usize,
    mode: VMInterruptMode,
) -> crate::AxVmResult<crate::InterruptFabric> {
    crate::InterruptFabric::with_sink(mode, Arc::new(LoongArchPchPicIrqSink { vm_id }))
}

/// Register the platform IRQ injector for LoongArch dynamic hypervisor builds.
pub(crate) fn register_platform_irq_injector() {
    ax_plat::irq::loongarch64_hv::register_virtual_irq_injector(inject_platform_irq);
    set_irq_enabled(EIOINTC_IRQ, true);
}

/// Route a host physical IRQ to a LoongArch guest interrupt vector.
pub fn register_guest_irq_route(
    physical_irq: usize,
    vm_id: usize,
    vcpu_id: usize,
    guest_vector: usize,
) {
    ax_plat::irq::loongarch64_hv::register_guest_irq_route(
        physical_irq,
        vm_id,
        vcpu_id,
        guest_vector,
    );
}

/// Remove all routed LoongArch guest IRQs owned by one VM.
pub fn unregister_guest_irq_routes(vm_id: usize) {
    ax_plat::irq::loongarch64_hv::unregister_guest_irq_routes(vm_id);
}

fn set_irq_enabled(raw_irq: usize, enabled: bool) {
    use ax_std::os::arceos::modules::ax_hal::irq::{self, IrqSource};

    let gsi = match u32::try_from(raw_irq) {
        Ok(gsi) => gsi,
        Err(_) => {
            warn!("failed to resolve LoongArch passthrough IRQ {raw_irq}: out of GSI range");
            return;
        }
    };
    let irq = match irq::resolve_irq_source(IrqSource::AcpiGsi(gsi)) {
        Ok(irq) => irq,
        Err(err) => {
            warn!("failed to resolve LoongArch passthrough IRQ {raw_irq}: {err:?}");
            return;
        }
    };
    if let Err(err) = irq::set_enable(irq, enabled) {
        warn!(
            "failed to set LoongArch passthrough IRQ {raw_irq} ({irq:?}) enabled={enabled}: \
             {err:?}"
        );
    }
}

fn inject_platform_irq(vm_id: usize, vcpu_id: usize, vector: usize, physical_irq: usize) {
    if let Err(err) =
        crate::runtime::vcpus::queue_external_interrupt(vm_id, vcpu_id, vector, physical_irq)
    {
        warn!(
            "failed to queue LoongArch platform IRQ {vector:#x}/physical {physical_irq:#x} for \
             VM[{vm_id}] VCpu[{vcpu_id}]: {err:?}"
        );
    }
}
