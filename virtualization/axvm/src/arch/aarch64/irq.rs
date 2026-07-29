//! AArch64 virtual-GIC interrupt routing.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use ax_kspin::SpinRaw as Mutex;
use axdevice_base::{InterruptTriggerMode, IrqLineId, IrqResult, IrqSink};
use axvm_types::VMInterruptMode;

use crate::{
    AxVmError, AxVmResult,
    config::PassthroughInterrupt,
    runtime::{VCpuRef, VMRef},
};

const GIC_SPI_BASE: u32 = 32;
const GIC_MAX_INTID: usize = 1020;
const UNASSIGNED_ROUTE: usize = usize::MAX;

type HostIrqHandle = ax_std::os::arceos::modules::ax_hal::irq::IrqHandle;

static PASSTHROUGH_IRQ_OWNER: [AtomicUsize; GIC_MAX_INTID] =
    [const { AtomicUsize::new(UNASSIGNED_ROUTE) }; GIC_MAX_INTID];
static PASSTHROUGH_IRQ_TARGET_VCPU: [AtomicUsize; GIC_MAX_INTID] =
    [const { AtomicUsize::new(UNASSIGNED_ROUTE) }; GIC_MAX_INTID];
static PASSTHROUGH_IRQ_LEVEL_TRIGGERED: [AtomicBool; GIC_MAX_INTID] =
    [const { AtomicBool::new(false) }; GIC_MAX_INTID];
static PASSTHROUGH_IRQ_ENABLED: [AtomicBool; GIC_MAX_INTID] =
    [const { AtomicBool::new(false) }; GIC_MAX_INTID];
static PASSTHROUGH_IRQ_PENDING: [AtomicBool; GIC_MAX_INTID] =
    [const { AtomicBool::new(false) }; GIC_MAX_INTID];
static PASSTHROUGH_IRQ_MASKED: [AtomicBool; GIC_MAX_INTID] =
    [const { AtomicBool::new(false) }; GIC_MAX_INTID];
static PASSTHROUGH_IRQ_HANDLES: [Mutex<Option<HostIrqHandle>>; GIC_MAX_INTID] =
    [const { Mutex::new(None) }; GIC_MAX_INTID];

struct Aarch64VirtualGicIrqSink {
    vm_id: usize,
    vcpu_id: usize,
}

impl IrqSink for Aarch64VirtualGicIrqSink {
    fn set_level(&self, line: IrqLineId, asserted: bool) -> IrqResult {
        if asserted {
            self.dispatch(line, InterruptTriggerMode::LevelTriggered)
        } else {
            Ok(())
        }
    }

    fn pulse(&self, line: IrqLineId) -> IrqResult {
        self.dispatch(line, InterruptTriggerMode::EdgeTriggered)
    }
}

impl Aarch64VirtualGicIrqSink {
    fn dispatch(&self, line: IrqLineId, trigger: InterruptTriggerMode) -> IrqResult {
        crate::irq::dispatch_runtime_interrupt(self.vm_id, self.vcpu_id, line, line.0, trigger)
    }
}

/// Routes machine interrupt IDs through the target vCPU's virtual-GIC queue.
pub(super) fn interrupt_fabric(
    vm_id: usize,
    mode: VMInterruptMode,
) -> crate::AxVmResult<crate::InterruptFabric> {
    crate::InterruptFabric::with_sink(
        mode,
        Arc::new(Aarch64VirtualGicIrqSink { vm_id, vcpu_id: 0 }),
    )
}

/// Registers physical SPI actions without assigning host GIC state to a guest.
///
/// Actions remain disabled until the target vCPU starts. The hard-IRQ handler
/// only latches pending state; actual virtual-GIC injection happens while the
/// target vCPU is pinned in [`drain_passthrough_irqs`].
pub(super) fn prepare_passthrough_irq_routes(
    vm_id: usize,
    target_vcpu_id: usize,
    target_cpu_id: usize,
    routes: &[PassthroughInterrupt],
) -> AxVmResult {
    use ax_std::os::arceos::modules::ax_hal::irq::{
        self as host_irq, AutoEnable, CpuId, IrqAffinity, IrqRequest, IrqSource, ShareMode,
    };

    for route in routes {
        let intid = route
            .source
            .checked_add(GIC_SPI_BASE)
            .and_then(|intid| usize::try_from(intid).ok())
            .filter(|intid| *intid < GIC_MAX_INTID)
            .ok_or_else(|| {
                AxVmError::invalid_config(alloc::format!(
                    "AArch64 physical SPI {} is outside the injectable GIC range",
                    route.source
                ))
            })?;

        if let Err(existing_vm_id) = PASSTHROUGH_IRQ_OWNER[intid].compare_exchange(
            UNASSIGNED_ROUTE,
            vm_id,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            release_passthrough_irq_routes(vm_id);
            return Err(AxVmError::resource_conflict(
                "AArch64 physical IRQ",
                alloc::format!("INTID {intid} is already assigned to VM[{existing_vm_id}]"),
            ));
        }

        PASSTHROUGH_IRQ_TARGET_VCPU[intid].store(target_vcpu_id, Ordering::Release);
        PASSTHROUGH_IRQ_LEVEL_TRIGGERED[intid].store(
            route.trigger == InterruptTriggerMode::LevelTriggered,
            Ordering::Release,
        );
        PASSTHROUGH_IRQ_PENDING[intid].store(false, Ordering::Release);
        PASSTHROUGH_IRQ_MASKED[intid].store(false, Ordering::Release);

        let host_irq = match host_irq::resolve_irq_source(IrqSource::AcpiGsi(intid as u32)) {
            Ok(host_irq) => host_irq,
            Err(error) => {
                release_passthrough_irq_routes(vm_id);
                return Err(AxVmError::interrupt(
                    "resolve AArch64 physical IRQ",
                    alloc::format!("INTID {intid}: {error:?}"),
                ));
            }
        };
        let request = IrqRequest::new_concurrent(passthrough_irq_handler)
            .affinity(IrqAffinity::Fixed(CpuId(target_cpu_id)))
            .share_mode(ShareMode::Shared)
            .auto_enable(AutoEnable::No);
        let handle = match host_irq::request_irq(host_irq, request) {
            Ok(handle) => handle,
            Err(error) => {
                release_passthrough_irq_routes(vm_id);
                return Err(AxVmError::interrupt(
                    "register AArch64 physical IRQ",
                    alloc::format!("INTID {intid}: {error:?}"),
                ));
            }
        };
        *PASSTHROUGH_IRQ_HANDLES[intid].lock() = Some(handle);
    }

    Ok(())
}

pub(super) fn enable_passthrough_irq_routes(vm_id: usize, vcpu_id: usize) {
    use ax_std::os::arceos::modules::ax_hal::irq::enable_irq;

    for intid in passthrough_intids_for(vm_id, vcpu_id) {
        if PASSTHROUGH_IRQ_ENABLED[intid].swap(true, Ordering::AcqRel) {
            continue;
        }
        let Some(handle) = *PASSTHROUGH_IRQ_HANDLES[intid].lock() else {
            PASSTHROUGH_IRQ_ENABLED[intid].store(false, Ordering::Release);
            continue;
        };
        if let Err(error) = enable_irq(handle) {
            PASSTHROUGH_IRQ_ENABLED[intid].store(false, Ordering::Release);
            warn!("failed to enable AArch64 physical IRQ INTID {intid} for VM[{vm_id}]: {error:?}");
        }
    }
}

pub(super) fn drain_passthrough_irqs(vm: &VMRef, vcpu: &VCpuRef) {
    use ax_std::os::arceos::modules::ax_hal::irq::set_enable;

    for intid in passthrough_intids_for(vm.id(), vcpu.id()) {
        if !PASSTHROUGH_IRQ_PENDING[intid].swap(false, Ordering::AcqRel) {
            continue;
        }
        let trigger = if PASSTHROUGH_IRQ_LEVEL_TRIGGERED[intid].load(Ordering::Acquire) {
            InterruptTriggerMode::LevelTriggered
        } else {
            InterruptTriggerMode::EdgeTriggered
        };
        if let Err(error) = vcpu.inject_interrupt_with_trigger(intid, trigger) {
            PASSTHROUGH_IRQ_PENDING[intid].store(true, Ordering::Release);
            warn!(
                "VM[{}] VCpu[{}] failed to inject AArch64 physical IRQ INTID {intid}: {error:?}",
                vm.id(),
                vcpu.id()
            );
            continue;
        }

        if PASSTHROUGH_IRQ_MASKED[intid].swap(false, Ordering::AcqRel) {
            let Some(handle) = *PASSTHROUGH_IRQ_HANDLES[intid].lock() else {
                continue;
            };
            if let Err(error) = set_enable(handle.irq(), true) {
                PASSTHROUGH_IRQ_MASKED[intid].store(true, Ordering::Release);
                warn!(
                    "failed to rearm AArch64 physical IRQ INTID {intid} after virtual injection: \
                     {error:?}"
                );
            }
        }
    }
}

pub(super) fn release_passthrough_irq_routes(vm_id: usize) {
    use ax_std::os::arceos::modules::ax_hal::irq::{disable_irq, free_irq, set_enable};

    for intid in 0..GIC_MAX_INTID {
        if PASSTHROUGH_IRQ_OWNER[intid].load(Ordering::Acquire) != vm_id {
            continue;
        }

        PASSTHROUGH_IRQ_ENABLED[intid].store(false, Ordering::Release);
        PASSTHROUGH_IRQ_PENDING[intid].store(false, Ordering::Release);
        if let Some(handle) = PASSTHROUGH_IRQ_HANDLES[intid].lock().take() {
            if PASSTHROUGH_IRQ_MASKED[intid].swap(false, Ordering::AcqRel) {
                let _ = set_enable(handle.irq(), true);
            }
            let _ = disable_irq(handle);
            if let Err(error) = free_irq(handle) {
                warn!(
                    "failed to release AArch64 physical IRQ INTID {intid} for VM[{vm_id}]: \
                     {error:?}"
                );
            }
        }
        PASSTHROUGH_IRQ_TARGET_VCPU[intid].store(UNASSIGNED_ROUTE, Ordering::Release);
        PASSTHROUGH_IRQ_LEVEL_TRIGGERED[intid].store(false, Ordering::Release);
        PASSTHROUGH_IRQ_OWNER[intid].store(UNASSIGNED_ROUTE, Ordering::Release);
    }
}

fn passthrough_intids_for(vm_id: usize, vcpu_id: usize) -> impl Iterator<Item = usize> {
    (GIC_SPI_BASE as usize..GIC_MAX_INTID).filter(move |intid| {
        PASSTHROUGH_IRQ_OWNER[*intid].load(Ordering::Acquire) == vm_id
            && PASSTHROUGH_IRQ_TARGET_VCPU[*intid].load(Ordering::Acquire) == vcpu_id
    })
}

fn passthrough_irq_handler(
    ctx: ax_std::os::arceos::modules::ax_hal::irq::IrqContext,
) -> ax_std::os::arceos::modules::ax_hal::irq::IrqReturn {
    use ax_std::os::arceos::modules::ax_hal::irq::{IrqReturn, set_enable};

    let intid = ctx.irq.hwirq.0 as usize;
    if intid >= GIC_MAX_INTID
        || PASSTHROUGH_IRQ_OWNER[intid].load(Ordering::Acquire) == UNASSIGNED_ROUTE
        || !PASSTHROUGH_IRQ_ENABLED[intid].load(Ordering::Acquire)
    {
        return IrqReturn::Unhandled;
    }

    if PASSTHROUGH_IRQ_LEVEL_TRIGGERED[intid].load(Ordering::Acquire)
        && !PASSTHROUGH_IRQ_MASKED[intid].swap(true, Ordering::AcqRel)
        && let Err(error) = set_enable(ctx.irq, false)
    {
        PASSTHROUGH_IRQ_MASKED[intid].store(false, Ordering::Release);
        warn!("failed to mask AArch64 physical IRQ INTID {intid}: {error:?}");
        return IrqReturn::Unhandled;
    }
    PASSTHROUGH_IRQ_PENDING[intid].store(true, Ordering::Release);
    IrqReturn::Handled
}
