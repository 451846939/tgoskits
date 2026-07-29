//! AArch64 virtual-GIC interrupt routing.

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use ax_kspin::SpinRaw as Mutex;
use axdevice::{
    DeviceBuildContext, DeviceBundle, DeviceFactory, DeviceFactoryRegistry, DeviceManagerError,
    DeviceManagerResult, DeviceRegistration, MmioDeviceAdapter,
};
use axdevice_base::{InterruptTriggerMode, IrqError, IrqLineId, IrqResult, IrqSink};
use axvm_types::{EmulatedDeviceConfig, EmulatedDeviceType, VMInterruptMode};

use crate::{
    AxVmError, AxVmResult,
    config::PassthroughInterrupt,
    runtime::{VCpuRef, VMRef},
};

const GIC_SPI_BASE: u32 = 32;
const GIC_MAX_INTID: usize = 1020;
const GIC_AFFINITY_MASK: u64 = 0x0000_00ff_00ff_ffff;
const GICD_IROUTER_IRM: u64 = 1 << 31;
const VGIC_DISTRIBUTOR_SIZE: usize = 0x1_0000;
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
    vcpu_affinities: Vec<(usize, Option<usize>, usize)>,
    vgic: Arc<arm_vgic::Vgic>,
}

impl IrqSink for Aarch64VirtualGicIrqSink {
    fn set_level(&self, line: IrqLineId, asserted: bool) -> IrqResult {
        let intid = u32::try_from(line.0).map_err(|_| IrqError::InvalidLine {
            line,
            operation: "set virtual GIC line level",
            detail: alloc::format!("INTID {} does not fit u32", line.0),
        })?;
        let should_dispatch = self
            .vgic
            .set_irq_line_level(intid, asserted)
            .map_err(|error| match error {
                arm_vgic::VgicError::InvalidIrq { .. } => IrqError::InvalidLine {
                    line,
                    operation: "set virtual GIC line level",
                    detail: alloc::format!("{error}"),
                },
                _ => IrqError::Backend {
                    line,
                    operation: "set virtual GIC line level",
                    detail: alloc::format!("{error}"),
                },
            })?;
        if should_dispatch {
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
        let intid = u32::try_from(line.0).map_err(|_| IrqError::InvalidLine {
            line,
            operation: "route virtual GIC interrupt",
            detail: alloc::format!("INTID {} does not fit u32", line.0),
        })?;
        let route = self
            .vgic
            .irq_route(intid)
            .map_err(|error| IrqError::Backend {
                line,
                operation: "route virtual GIC interrupt",
                detail: alloc::format!("{error}"),
            })?;
        let target_vcpu = target_vcpu_for_route(&self.vcpu_affinities, route).ok_or_else(|| {
            IrqError::Backend {
                line,
                operation: "route virtual GIC interrupt",
                detail: alloc::format!(
                    "GICD_IROUTER affinity {:#x} does not select a guest vCPU",
                    route & GIC_AFFINITY_MASK
                ),
            }
        })?;
        crate::irq::dispatch_runtime_interrupt(self.vm_id, target_vcpu, line, line.0, trigger)
    }
}

fn target_vcpu_for_route(
    vcpu_affinities: &[(usize, Option<usize>, usize)],
    route: u64,
) -> Option<usize> {
    if route & GICD_IROUTER_IRM != 0 {
        return vcpu_affinities.iter().map(|(vcpu_id, ..)| *vcpu_id).min();
    }

    let target_affinity = route & GIC_AFFINITY_MASK;
    vcpu_affinities
        .iter()
        .find(|(_, _, phys_cpu_id)| (*phys_cpu_id as u64 & GIC_AFFINITY_MASK) == target_affinity)
        .map(|(vcpu_id, ..)| *vcpu_id)
}

fn target_vcpu_for_irq(vm: &VMRef, vgic: &arm_vgic::Vgic, intid: usize) -> Option<usize> {
    let intid = u32::try_from(intid).ok()?;
    let route = vgic.irq_route(intid).ok()?;
    target_vcpu_for_route(&vm.get_vcpu_affinities_pcpu_ids(), route)
}

struct Aarch64VgicFactory {
    base_gpa: usize,
    length: usize,
    vgic: Arc<arm_vgic::Vgic>,
}

impl DeviceFactory for Aarch64VgicFactory {
    fn device_type(&self) -> EmulatedDeviceType {
        EmulatedDeviceType::InterruptController
    }

    fn build(
        &self,
        config: &EmulatedDeviceConfig,
        _context: &DeviceBuildContext<'_>,
    ) -> DeviceManagerResult<DeviceBundle> {
        if config.base_gpa != self.base_gpa
            || config.length != self.length
            || !config.cfg_list.is_empty()
        {
            return Err(DeviceManagerError::InvalidConfig {
                operation: "build virtual GIC distributor",
                detail: alloc::format!(
                    "factory configuration does not match device '{}'",
                    config.name
                ),
            });
        }
        Ok(DeviceRegistration::Device(MmioDeviceAdapter::from_arc(self.vgic.clone())).into())
    }
}

fn validate_vgic_config(config: &EmulatedDeviceConfig) -> AxVmResult {
    if config.length < VGIC_DISTRIBUTOR_SIZE {
        return Err(AxVmError::invalid_config(alloc::format!(
            "virtual GIC distributor '{}' length {:#x} is smaller than {VGIC_DISTRIBUTOR_SIZE:#x}",
            config.name,
            config.length
        )));
    }
    if !config.cfg_list.is_empty() {
        return Err(AxVmError::invalid_config(alloc::format!(
            "virtual GIC distributor '{}' does not accept configuration arguments",
            config.name
        )));
    }
    Ok(())
}

/// Creates one shared virtual GIC for the MMIO device and IRQ-line backend.
pub(super) fn configure(
    factories: &mut DeviceFactoryRegistry,
    vm: &crate::AxVM,
    mode: VMInterruptMode,
    configs: &[EmulatedDeviceConfig],
) -> crate::AxVmResult<crate::InterruptFabric> {
    let mut vgic_configs = configs
        .iter()
        .filter(|config| config.emu_type == EmulatedDeviceType::InterruptController);
    let Some(config) = vgic_configs.next() else {
        return Ok(crate::InterruptFabric::new(mode));
    };
    if vgic_configs.next().is_some() {
        return Err(AxVmError::invalid_config(
            "a VM can register only one virtual GIC distributor",
        ));
    }

    validate_vgic_config(config)?;
    let vgic = Arc::new(arm_vgic::Vgic::new_at(
        config.base_gpa.into(),
        config.length,
    ));
    factories.register(Arc::new(Aarch64VgicFactory {
        base_gpa: config.base_gpa,
        length: config.length,
        vgic: vgic.clone(),
    }))?;
    let vcpu_affinities =
        vm.with_config(|config| config.phys_cpu_ls.get_vcpu_affinities_pcpu_ids());
    debug!(
        "VM[{}] virtual GIC affinity routes: {vcpu_affinities:?}",
        vm.id()
    );

    crate::InterruptFabric::with_sink(
        mode,
        Arc::new(Aarch64VirtualGicIrqSink {
            vm_id: vm.id(),
            vcpu_affinities,
            vgic,
        }),
    )
}

/// Redelivers enabled level inputs that remain asserted after guest EOI.
pub(super) fn reinject_asserted_virtual_irqs(vm: &VMRef, vcpu: &VCpuRef) {
    let Ok(devices) = vm.get_devices() else {
        return;
    };
    let Some(vgic) = devices
        .devices()
        .find_map(|device| device.as_any().downcast_ref::<arm_vgic::Vgic>())
    else {
        return;
    };

    for intid in vgic.asserted_enabled_irqs() {
        if target_vcpu_for_irq(vm, vgic, intid as usize) != Some(vcpu.id()) {
            continue;
        }
        let intid = intid as usize;
        if !super::gic::virtual_interrupt_inactive(intid) {
            continue;
        }
        if let Err(error) =
            vcpu.inject_interrupt_with_trigger(intid, InterruptTriggerMode::LevelTriggered)
        {
            warn!(
                "VM[{}] VCpu[{}] failed to reinject asserted virtual IRQ INTID {intid}: {error:?}",
                vm.id(),
                vcpu.id()
            );
        }
    }
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
            .filter(|intid| *intid < GIC_MAX_INTID && *intid < arm_vgic::Vgic::MAX_INTID_EXCLUSIVE)
            .ok_or_else(|| {
                AxVmError::invalid_config(alloc::format!(
                    "AArch64 physical SPI {} is outside the virtual GIC range 0..{}",
                    route.source,
                    arm_vgic::Vgic::MAX_INTID_EXCLUSIVE
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

/// Synchronizes physical passthrough lines with the guest distributor mask.
///
/// Registering a physical route does not authorize delivery. The guest must
/// first enable the matching INTID in its virtual GIC distributor.
pub(super) fn sync_passthrough_irq_routes(vm: &VMRef, vcpu: &VCpuRef) {
    use ax_std::os::arceos::modules::ax_hal::irq::{disable_irq, enable_irq};

    let Ok(devices) = vm.get_devices() else {
        return;
    };
    let Some(vgic) = devices
        .devices()
        .find_map(|device| device.as_any().downcast_ref::<arm_vgic::Vgic>())
    else {
        return;
    };

    for intid in passthrough_intids_for_vm(vm.id()) {
        let Some(target_vcpu_id) = target_vcpu_for_irq(vm, vgic, intid) else {
            PASSTHROUGH_IRQ_TARGET_VCPU[intid].store(UNASSIGNED_ROUTE, Ordering::Release);
            PASSTHROUGH_IRQ_PENDING[intid].store(false, Ordering::Release);
            PASSTHROUGH_IRQ_MASKED[intid].store(false, Ordering::Release);
            if PASSTHROUGH_IRQ_ENABLED[intid].swap(false, Ordering::AcqRel)
                && let Some(handle) = *PASSTHROUGH_IRQ_HANDLES[intid].lock()
                && let Err(error) = disable_irq(handle)
            {
                PASSTHROUGH_IRQ_ENABLED[intid].store(true, Ordering::Release);
                warn!(
                    "failed to disable unrouted AArch64 physical IRQ INTID {intid} for VM[{}]: \
                     {error:?}",
                    vm.id()
                );
            }
            continue;
        };
        PASSTHROUGH_IRQ_TARGET_VCPU[intid].store(target_vcpu_id, Ordering::Release);
        if target_vcpu_id != vcpu.id() {
            continue;
        }

        let guest_enabled = vgic.irq_enabled(intid as u32);
        let should_enable = guest_enabled;
        if PASSTHROUGH_IRQ_ENABLED[intid].load(Ordering::Acquire) == should_enable {
            continue;
        }
        let Some(handle) = *PASSTHROUGH_IRQ_HANDLES[intid].lock() else {
            continue;
        };

        if should_enable {
            PASSTHROUGH_IRQ_ENABLED[intid].store(true, Ordering::Release);
            if let Err(error) = enable_irq(handle) {
                PASSTHROUGH_IRQ_ENABLED[intid].store(false, Ordering::Release);
                warn!(
                    "failed to enable AArch64 physical IRQ INTID {intid} for VM[{}]: {error:?}",
                    vm.id()
                );
            }
        } else {
            PASSTHROUGH_IRQ_ENABLED[intid].store(false, Ordering::Release);
            PASSTHROUGH_IRQ_PENDING[intid].store(false, Ordering::Release);
            PASSTHROUGH_IRQ_MASKED[intid].store(false, Ordering::Release);
            if let Err(error) = disable_irq(handle) {
                PASSTHROUGH_IRQ_ENABLED[intid].store(true, Ordering::Release);
                warn!(
                    "failed to disable AArch64 physical IRQ INTID {intid} for VM[{}]: {error:?}",
                    vm.id()
                );
            }
        }
    }
}

pub(super) fn drain_passthrough_irqs(vm: &VMRef, vcpu: &VCpuRef) {
    use ax_std::os::arceos::modules::ax_hal::irq::set_enable;

    for intid in passthrough_intids_for(vm.id(), vcpu.id()) {
        if !PASSTHROUGH_IRQ_PENDING[intid].load(Ordering::Acquire) {
            continue;
        }
        if !PASSTHROUGH_IRQ_ENABLED[intid].load(Ordering::Acquire) {
            PASSTHROUGH_IRQ_PENDING[intid].store(false, Ordering::Release);
            continue;
        }
        let level_triggered = PASSTHROUGH_IRQ_LEVEL_TRIGGERED[intid].load(Ordering::Acquire);
        if level_triggered {
            let Some(handle) = *PASSTHROUGH_IRQ_HANDLES[intid].lock() else {
                continue;
            };
            if let Err(error) = set_enable(handle.irq(), false) {
                warn!(
                    "failed to mask AArch64 physical IRQ INTID {intid} before vGIC injection: \
                     {error:?}"
                );
                continue;
            }
        }
        if !PASSTHROUGH_IRQ_PENDING[intid].swap(false, Ordering::AcqRel) {
            continue;
        }
        let trigger = if level_triggered {
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
    }
}

/// Rearms a level-triggered physical line only after the guest deactivated
/// the corresponding virtual interrupt.
pub(super) fn rearm_passthrough_irq_routes_after_eoi(vm: &VMRef, vcpu: &VCpuRef) {
    use ax_std::os::arceos::modules::ax_hal::irq::set_enable;

    for intid in passthrough_intids_for(vm.id(), vcpu.id()) {
        if !PASSTHROUGH_IRQ_ENABLED[intid].load(Ordering::Acquire)
            || !PASSTHROUGH_IRQ_MASKED[intid].load(Ordering::Acquire)
            || PASSTHROUGH_IRQ_PENDING[intid].load(Ordering::Acquire)
            || !super::gic::virtual_interrupt_inactive(intid)
        {
            continue;
        }
        if PASSTHROUGH_IRQ_MASKED[intid]
            .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            continue;
        }
        let Some(handle) = *PASSTHROUGH_IRQ_HANDLES[intid].lock() else {
            continue;
        };
        if let Err(error) = set_enable(handle.irq(), true) {
            PASSTHROUGH_IRQ_MASKED[intid].store(true, Ordering::Release);
            warn!("failed to rearm AArch64 physical IRQ INTID {intid} after guest EOI: {error:?}");
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
    passthrough_intids_for_vm(vm_id)
        .filter(move |intid| PASSTHROUGH_IRQ_TARGET_VCPU[*intid].load(Ordering::Acquire) == vcpu_id)
}

fn passthrough_intids_for_vm(vm_id: usize) -> impl Iterator<Item = usize> {
    (GIC_SPI_BASE as usize..GIC_MAX_INTID)
        .filter(move |intid| PASSTHROUGH_IRQ_OWNER[*intid].load(Ordering::Acquire) == vm_id)
}

fn passthrough_irq_handler(
    ctx: ax_std::os::arceos::modules::ax_hal::irq::IrqContext,
) -> ax_std::os::arceos::modules::ax_hal::irq::IrqReturn {
    use ax_std::os::arceos::modules::ax_hal::irq::IrqReturn;

    let intid = ctx.irq.hwirq.0 as usize;
    if intid >= GIC_MAX_INTID
        || PASSTHROUGH_IRQ_OWNER[intid].load(Ordering::Acquire) == UNASSIGNED_ROUTE
        || !PASSTHROUGH_IRQ_ENABLED[intid].load(Ordering::Acquire)
    {
        return IrqReturn::Unhandled;
    }

    PASSTHROUGH_IRQ_PENDING[intid].store(true, Ordering::Release);
    if PASSTHROUGH_IRQ_LEVEL_TRIGGERED[intid].load(Ordering::Acquire) {
        // The host GIC controller lock is held while hard-IRQ actions run.
        // Record mask ownership here and perform the actual distributor write
        // from the pinned vCPU path before injecting the same INTID.
        PASSTHROUGH_IRQ_MASKED[intid].store(true, Ordering::Release);
    }
    let vm_id = PASSTHROUGH_IRQ_OWNER[intid].load(Ordering::Acquire);
    let target_vcpu_id = PASSTHROUGH_IRQ_TARGET_VCPU[intid].load(Ordering::Acquire);
    if target_vcpu_id != UNASSIGNED_ROUTE
        && let Err(error) = crate::notify_vm_vcpu(vm_id, target_vcpu_id)
    {
        warn!(
            "failed to wake VM[{vm_id}] VCpu[{target_vcpu_id}] for physical IRQ INTID {intid}: \
             {error:?}"
        );
    }
    IrqReturn::Handled
}
