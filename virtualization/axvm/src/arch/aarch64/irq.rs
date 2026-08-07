//! AArch64 VM-local interrupt backend.

use axdevice_base::{IrqError, IrqLineId, IrqResult, IrqSink};
use axvm_types::{VMId, VMInterruptMode};

use crate::{AxVmResult, irq::InterruptFabric};

struct Aarch64VmIrqSink {
    vm_id: VMId,
    mode: VMInterruptMode,
    target_vcpu_id: usize,
}

impl IrqSink for Aarch64VmIrqSink {
    fn set_level(&self, line: IrqLineId, asserted: bool) -> IrqResult {
        if self.mode == VMInterruptMode::Passthrough {
            super::gic::set_physical_irq_pending(line.0, asserted);
            return Ok(());
        }

        if asserted {
            self.pulse(line)?;
        }
        Ok(())
    }

    fn pulse(&self, line: IrqLineId) -> IrqResult {
        if self.mode == VMInterruptMode::Passthrough {
            super::gic::set_physical_irq_pending(line.0, true);
            return Ok(());
        }

        crate::manager::inject_interrupt(self.vm_id, self.target_vcpu_id, line.0).map_err(|error| {
            IrqError::Backend {
                line,
                operation: "pulse AArch64 VM IRQ line",
                detail: alloc::format!("{error}"),
            }
        })
    }
}

pub(crate) fn configure(vm_id: VMId, mode: VMInterruptMode) -> AxVmResult<InterruptFabric> {
    if mode == VMInterruptMode::NoIrq {
        return Ok(InterruptFabric::new(mode));
    }

    InterruptFabric::with_sink(
        mode,
        alloc::sync::Arc::new(Aarch64VmIrqSink {
            vm_id,
            mode,
            target_vcpu_id: 0,
        }),
    )
}
