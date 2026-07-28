//! VM-local interrupt sink that routes device IRQ pulses into a target vCPU's
//! pending queue.
//!
//! This is the architecture-independent counterpart to direct GIC
//! list-register writes: a pulse here is recorded against the owning VM's
//! target vCPU and drained by the vCPU run loop while it is bound to a host CPU
//! (see `runtime::vcpus::inject_pending_interrupts`). RX/data-path workers that
//! run on arbitrary host CPUs may therefore safely pulse this sink; they never
//! touch virtualization system registers such as `ICH_LR_EL2` directly.

use alloc::sync::Weak;

use axdevice_base::{IrqError, IrqLineId, IrqResult, IrqSink};

use crate::AxVM;

/// Interrupt sink that queues edge pulses for a specific VM and prepare
/// generation.
#[derive(Clone)]
pub struct VmQueuedIrqSink {
    vm: Weak<AxVM>,
    generation: usize,
    target_vcpu: usize,
}

impl VmQueuedIrqSink {
    pub fn new(vm: Weak<AxVM>, generation: usize, target_vcpu: usize) -> Self {
        Self {
            vm,
            generation,
            target_vcpu,
        }
    }
}

impl IrqSink for VmQueuedIrqSink {
    fn set_level(&self, line: IrqLineId, _asserted: bool) -> IrqResult {
        Err(IrqError::Unsupported {
            line,
            operation: "set_level",
            detail: "level-triggered IRQs are not supported by the VM queued sink".into(),
        })
    }

    fn pulse(&self, line: IrqLineId) -> IrqResult {
        let Some(vm) = self.vm.upgrade() else {
            return Err(IrqError::InvalidLine {
                line,
                operation: "pulse",
                detail: "owning VM has been dropped".into(),
            });
        };

        if vm.prepare_generation() != self.generation {
            return Err(IrqError::InvalidLine {
                line,
                operation: "pulse",
                detail: alloc::format!(
                    "sink generation {} is stale; VM is now at generation {}",
                    self.generation,
                    vm.prepare_generation()
                ),
            });
        }

        crate::runtime::vcpus::queue_interrupt(vm.id(), self.target_vcpu, line.0).map_err(|err| {
            IrqError::Backend {
                line,
                operation: "pulse",
                detail: alloc::format!("{err:?}"),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropped_vm_rejects_pulse() {
        let sink = VmQueuedIrqSink::new(Weak::<AxVM>::new(), 1, 0);
        let result = sink.pulse(IrqLineId(65));
        assert!(matches!(result, Err(IrqError::InvalidLine { .. })));
        assert!(matches!(
            sink.set_level(IrqLineId(65), true),
            Err(IrqError::Unsupported { .. })
        ));
    }
}
