use alloc::boxed::Box;
use core::{
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use arm_vcpu::host::ArmVcpuHostIf;
use arm_vgic::host::ArmVgicHostIf;
use ax_crate_interface::impl_interface;
use ax_errno::{AxResult, ax_err};
use ax_memory_addr::{PhysAddr, VirtAddr};
use axvm_types::NestedPagingConfig;

use super::{ArchOps, VcpuCreateContext, VcpuSetupContext, target_phys_cpu_ids};
use crate::host::{HostCpu, HostMemory, HostTime, default_host, gic};

mod npt;

static IRQ_INJECTION_DIAGNOSTICS: AtomicUsize = AtomicUsize::new(0);

pub(crate) struct Aarch64Arch;

impl ArchOps for Aarch64Arch {
    type VCpu = arm_vcpu::Aarch64VCpu;
    type PerCpu = arm_vcpu::Aarch64PerCpu;
    type VcpuCreateState = ();
    type NestedPageTable = npt::NestedPageTable<crate::HostPagingHandler>;

    fn has_hardware_support() -> bool {
        arm_vcpu::has_hardware_support()
    }

    fn max_guest_page_table_levels() -> usize {
        arm_vcpu::max_guest_page_table_levels()
    }

    fn guest_page_table_levels(vcpu_mappings: &[(usize, Option<usize>, usize)]) -> AxResult<usize> {
        let mut selected = usize::MAX;
        for cpu_id in target_phys_cpu_ids(vcpu_mappings) {
            let levels = crate::percpu::cpu_max_guest_page_table_levels(cpu_id)
                .unwrap_or_else(arm_vcpu::max_guest_page_table_levels);
            if levels == 0 {
                return ax_err!(
                    Unsupported,
                    "AArch64 nested paging is not enabled on target CPU"
                );
            }
            selected = selected.min(levels);
        }
        if selected == usize::MAX {
            selected = arm_vcpu::max_guest_page_table_levels();
        }
        match selected {
            3 | 4 => Ok(selected),
            _ => ax_err!(Unsupported, "unsupported AArch64 stage-2 page-table levels"),
        }
    }

    fn nested_paging_config(
        root_paddr: PhysAddr,
        levels: usize,
        vcpu_mappings: &[(usize, Option<usize>, usize)],
    ) -> AxResult<NestedPagingConfig> {
        let mut pa_bits = usize::MAX;
        for cpu_id in target_phys_cpu_ids(vcpu_mappings) {
            let bits =
                crate::percpu::cpu_guest_phys_addr_bits(cpu_id).unwrap_or_else(arm_vcpu::pa_bits);
            pa_bits = pa_bits.min(bits);
        }
        if pa_bits == usize::MAX {
            pa_bits = arm_vcpu::pa_bits();
        }

        let gpa_bits = match levels {
            3 => 39,
            4 => 48,
            _ => return ax_err!(InvalidInput, "unsupported AArch64 stage-2 levels"),
        };
        Ok(NestedPagingConfig::new(
            root_paddr, levels, gpa_bits, pa_bits,
        ))
    }

    fn new_nested_page_table(levels: usize) -> AxResult<Self::NestedPageTable> {
        npt::NestedPageTable::new(levels)
    }

    fn clean_dcache_range(addr: VirtAddr, size: usize) {
        aarch64_cpu_ext::cache::dcache_range(
            aarch64_cpu_ext::cache::CacheOp::Clean,
            addr.as_usize(),
            size,
        );
    }

    fn on_vcpu_task_exit(vm_id: usize, vcpu_id: usize) {
        // CNTP/CNTV are physical per-CPU timer sources shared by the most
        // recently executed guest context. A normal VM-exit keeps them armed
        // so GIC hardware-backed interrupt completion remains intact, but a
        // permanently exiting vCPU must not leave either source active in a
        // subsequent Host task on the same pCPU.
        unsafe {
            core::arch::asm!(
                "msr CNTP_CTL_EL0, xzr",
                "msr CNTV_CTL_EL0, xzr",
                "isb",
                options(nostack, preserves_flags)
            );
        }
        info!("VM[{vm_id}] VCpu[{vcpu_id}] disabled guest timer sources on task exit");
    }

    fn new_vcpu_create_state(
        _vcpu_mappings: &[(usize, Option<usize>, usize)],
    ) -> AxResult<Self::VcpuCreateState> {
        Ok(())
    }

    fn build_vcpu_create_config(
        _state: &Self::VcpuCreateState,
        ctx: VcpuCreateContext,
    ) -> AxResult<<Self::VCpu as axvm_types::VmArchVcpuOps>::CreateConfig> {
        Ok(arm_vcpu::Aarch64VCpuCreateConfig {
            mpidr_el1: ctx.phys_cpu_id as _,
            dtb_addr: ctx.dtb_addr.unwrap_or_default().as_usize(),
        })
    }

    fn build_vcpu_setup_config(
        ctx: VcpuSetupContext<'_>,
    ) -> AxResult<<Self::VCpu as axvm_types::VmArchVcpuOps>::SetupConfig> {
        let passthrough = ctx.interrupt_mode == axvm_types::VMInterruptMode::Passthrough;
        Ok(arm_vcpu::Aarch64VCpuSetupConfig {
            // Preserve whether unclaimed physical interrupts may be forwarded
            // to this VM after EL2 ownership dispatch.
            passthrough_interrupt: passthrough,
            passthrough_timer: passthrough,
        })
    }

    fn register_platform_irq_injector() {
        crate::host::gic::init_backend();
        crate::irq::register_aarch64_virtual_irq_injector(inject_virtual_irq);
    }

    fn inject_pending_interrupt(
        vm: &crate::AxVM,
        vcpu: &crate::vm::AxVCpuRef,
        interrupt: crate::vm::PendingInterrupt,
    ) -> Option<crate::vm::PendingInterrupt> {
        match interrupt {
            crate::vm::PendingInterrupt::Normal(vector) => {
                trace!(
                    "Injecting queued interrupt {vector:#x} into VM[{}] VCpu[{}]",
                    vcpu.vm_id(),
                    vcpu.id()
                );
                if let Err(err) = vcpu.inject_interrupt(vector) {
                    warn!(
                        "Failed to inject queued interrupt {vector:#x} into VM[{}] VCpu[{}]: \
                         {err:?}",
                        vcpu.vm_id(),
                        vcpu.id()
                    );
                }
                None
            }
            crate::vm::PendingInterrupt::Replay(vector) => match gic::inject_interrupt(vector) {
                gic::VirtualIrqInjection::Injected => None,
                gic::VirtualIrqInjection::AlreadyPending | gic::VirtualIrqInjection::Deferred => {
                    Some(crate::vm::PendingInterrupt::Replay(vector))
                }
            },
            crate::vm::PendingInterrupt::External {
                vector,
                physical_irq,
                priority,
            } => {
                let Some(priority) = priority else {
                    crate::host::gic::deactivate_deferred_physical_interrupt(physical_irq);
                    warn!(
                        "Queued AArch64 external interrupt vector={vector:#x}, physical \
                         IRQ={physical_irq:#x} has no priority"
                    );
                    return None;
                };
                match crate::host::gic::inject_routed_physical_interrupt(
                    vector,
                    physical_irq,
                    priority,
                ) {
                    gic::PhysicalIrqInjection::Injected => None,
                    gic::PhysicalIrqInjection::ReplayRequired => {
                        crate::host::gic::deactivate_deferred_physical_interrupt(physical_irq);
                        Some(crate::vm::PendingInterrupt::Replay(vector))
                    }
                    gic::PhysicalIrqInjection::Deferred => {
                        Some(crate::vm::PendingInterrupt::External {
                            vector,
                            physical_irq,
                            priority: Some(priority),
                        })
                    }
                    gic::PhysicalIrqInjection::Rejected => {
                        crate::host::gic::deactivate_deferred_physical_interrupt(physical_irq);
                        warn!(
                            "Rejected queued AArch64 external interrupt vector={vector:#x}, \
                             physical IRQ={physical_irq:#x} for VM[{}] VCpu[{}]",
                            vm.id(),
                            vcpu.id()
                        );
                        None
                    }
                }
            }
        }
    }

    fn after_external_interrupt(
        _vm: &crate::AxVMRef,
        _vcpu: &crate::vm::AxVCpuRef,
        _vector: usize,
    ) {
        // `fetch_irq()` already acknowledges and dispatches the physical GIC
        // interrupt. Dispatching again would consume an unrelated pending IRQ.
        crate::check_timer_events();
    }

    fn handle_idle(_vm: &crate::AxVMRef, _vcpu: &crate::vm::AxVCpuRef) {
        crate::check_timer_events();
        crate::host::task::yield_now();
    }

    fn ipi_targets(
        vm: &crate::AxVMRef,
        current_vcpu_id: usize,
        target_cpu: u64,
        target_cpu_aux: u64,
        send_to_all: bool,
        send_to_self: bool,
    ) -> crate::CpuMask<64> {
        let mut targets = crate::CpuMask::new();
        if send_to_all {
            for vcpu in vm.vcpu_list() {
                if vcpu.id() != current_vcpu_id {
                    targets.set(vcpu.id(), true);
                }
            }
        } else if send_to_self {
            targets.set(current_vcpu_id, true);
        } else {
            for (vcpu_id, _, phys_id) in vm.get_vcpu_affinities_pcpu_ids() {
                let affinity = phys_id as u64;
                let aff0 = affinity & 0xff;
                let aff123 = affinity & !0xff;
                if aff123 == target_cpu && aff0 < 16 && (target_cpu_aux & (1u64 << aff0)) != 0 {
                    targets.set(vcpu_id, true);
                }
            }
        }
        targets
    }
}

fn inject_virtual_irq(irq_id: usize, priority: u8) -> crate::irq::GuestIrqInjection {
    use crate::irq::GuestIrqInjection;

    if irq_id < 32 {
        return inject_private_irq(irq_id, priority);
    }

    let Some(route) = crate::runtime::aarch64_irq::guest_irq_route(irq_id) else {
        log_irq_injection_failure(irq_id, "no guest owns this physical IRQ");
        return GuestIrqInjection::NotHandled;
    };

    #[cfg(feature = "rt-shared-wait-baseline")]
    let vm = {
        let Some(vm) = crate::manager::get_vm_by_id(route.vm_id) else {
            crate::host::arceos::set_aarch64_irq_enabled(irq_id, false);
            log_irq_injection_failure(irq_id, "owning VM is absent from the runtime registry");
            return GuestIrqInjection::NotHandled;
        };
        if !accepts_passthrough_interrupts(&vm) {
            crate::host::arceos::set_aarch64_irq_enabled(irq_id, false);
            log_irq_injection_failure(
                irq_id,
                "owning VM is not accepting passthrough interrupts; physical IRQ masked",
            );
            return GuestIrqInjection::NotHandled;
        }
        vm
    };

    use crate::AsVCpuTask;

    let current = crate::host::task::current_task();
    if let Some(task) = current.try_as_vcpu_task()
        && task.vm().id() == route.vm_id
        && task.vcpu.id() == route.vcpu_id
    {
        #[cfg(not(feature = "rt-shared-wait-baseline"))]
        let vm = task.vm();
        #[cfg(not(feature = "rt-shared-wait-baseline"))]
        if !accepts_passthrough_interrupts(&vm) {
            crate::host::arceos::set_aarch64_irq_enabled(irq_id, false);
            log_irq_injection_failure(
                irq_id,
                "owning VM is not accepting passthrough interrupts; physical IRQ masked",
            );
            return GuestIrqInjection::NotHandled;
        }
        match crate::host::gic::inject_routed_physical_interrupt(
            route.guest_vector,
            irq_id,
            priority,
        ) {
            gic::PhysicalIrqInjection::Injected => return GuestIrqInjection::HardwareForwarded,
            gic::PhysicalIrqInjection::ReplayRequired => {
                if let Err(err) = crate::runtime::vcpus::queue_replay_interrupt_for_vm(
                    &vm,
                    route.vcpu_id,
                    route.guest_vector,
                ) {
                    warn!(
                        "failed to queue AArch64 replay IRQ {irq_id} for VM[{}] VCpu[{}]: {err:?}",
                        route.vm_id, route.vcpu_id
                    );
                }
                return GuestIrqInjection::Emulated;
            }
            gic::PhysicalIrqInjection::Deferred => {}
            gic::PhysicalIrqInjection::Rejected => {
                log_irq_injection_failure(irq_id, "GIC rejected the physical IRQ mapping");
                return GuestIrqInjection::NotHandled;
            }
        }
    }

    #[cfg(not(feature = "rt-shared-wait-baseline"))]
    let Some(vm) = crate::manager::get_vm_by_id(route.vm_id) else {
        crate::host::arceos::set_aarch64_irq_enabled(irq_id, false);
        log_irq_injection_failure(irq_id, "owning VM is absent from the runtime registry");
        return GuestIrqInjection::NotHandled;
    };

    match crate::runtime::vcpus::queue_external_interrupt_for_vm(
        &vm,
        route.vcpu_id,
        route.guest_vector,
        irq_id,
        Some(priority),
    ) {
        Ok(()) => GuestIrqInjection::HardwareForwarded,
        Err(err) => {
            crate::host::arceos::set_aarch64_irq_enabled(irq_id, false);
            warn!(
                "failed to queue AArch64 physical IRQ {irq_id} for VM[{}] VCpu[{}]; physical IRQ \
                 masked: {err:?}",
                route.vm_id, route.vcpu_id
            );
            GuestIrqInjection::NotHandled
        }
    }
}

fn inject_private_irq(irq_id: usize, priority: u8) -> crate::irq::GuestIrqInjection {
    use crate::{AsVCpuTask, irq::GuestIrqInjection};

    let current = crate::host::task::current_task();
    let current_target = current
        .try_as_vcpu_task()
        .map(|task| (task.vm(), task.vcpu.id()));

    let Some((vm, vcpu_id)) = current_target else {
        let pcpu_id = default_host().this_cpu_id();
        let Some(target) = crate::runtime::aarch64_irq::guest_private_irq_target(pcpu_id) else {
            log_irq_injection_failure(
                irq_id,
                "private IRQ arrived outside a vCPU task and this pCPU has no guest owner",
            );
            return GuestIrqInjection::NotHandled;
        };
        let Some(vm) = crate::manager::get_vm_by_id(target.vm_id) else {
            log_irq_injection_failure(
                irq_id,
                "private IRQ pCPU owner is absent from the runtime registry",
            );
            return GuestIrqInjection::NotHandled;
        };
        #[cfg(feature = "rt-shared-wait-baseline")]
        if !accepts_passthrough_interrupts(&vm) {
            log_irq_injection_failure(
                irq_id,
                "private IRQ pCPU owner is not accepting passthrough interrupts",
            );
            return GuestIrqInjection::NotHandled;
        }
        return match crate::runtime::vcpus::queue_external_interrupt_for_vm(
            &vm,
            target.vcpu_id,
            irq_id,
            irq_id,
            Some(priority),
        ) {
            Ok(()) => GuestIrqInjection::HardwareForwarded,
            Err(err) => {
                warn!(
                    "failed to queue AArch64 private IRQ {irq_id} from pCPU {pcpu_id} for VM[{}] \
                     VCpu[{}]: {err:?}",
                    target.vm_id, target.vcpu_id
                );
                return GuestIrqInjection::NotHandled;
            }
        };
    };

    if !accepts_passthrough_interrupts(&vm) {
        log_irq_injection_failure(irq_id, "current VM is not accepting passthrough interrupts");
        return GuestIrqInjection::NotHandled;
    }
    match crate::host::gic::inject_routed_physical_interrupt(irq_id, irq_id, priority) {
        gic::PhysicalIrqInjection::Injected => GuestIrqInjection::HardwareForwarded,
        gic::PhysicalIrqInjection::ReplayRequired => {
            if let Err(err) =
                crate::runtime::vcpus::queue_replay_interrupt_for_vm(&vm, vcpu_id, irq_id)
            {
                warn!(
                    "failed to queue AArch64 private replay IRQ {irq_id} for VM[{}] \
                     VCpu[{vcpu_id}]: {err:?}",
                    vm.id()
                );
            }
            GuestIrqInjection::Emulated
        }
        gic::PhysicalIrqInjection::Deferred => {
            match crate::runtime::vcpus::queue_external_interrupt_for_vm(
                &vm,
                vcpu_id,
                irq_id,
                irq_id,
                Some(priority),
            ) {
                Ok(()) => GuestIrqInjection::HardwareForwarded,
                Err(err) => {
                    warn!(
                        "failed to defer AArch64 private IRQ {irq_id} for VM[{}] VCpu[{vcpu_id}]: \
                         {err:?}",
                        vm.id()
                    );
                    GuestIrqInjection::NotHandled
                }
            }
        }
        gic::PhysicalIrqInjection::Rejected => {
            log_irq_injection_failure(irq_id, "GIC rejected the private IRQ mapping");
            GuestIrqInjection::NotHandled
        }
    }
}

#[cfg(feature = "rt-shared-wait-baseline")]
fn accepts_passthrough_interrupts(vm: &crate::AxVM) -> bool {
    vm.interrupt_mode() == axvm_types::VMInterruptMode::Passthrough
        && matches!(
            vm.status(),
            crate::VmStatus::Running | crate::VmStatus::Paused
        )
}

#[cfg(not(feature = "rt-shared-wait-baseline"))]
fn accepts_passthrough_interrupts(vm: &crate::AxVM) -> bool {
    vm.accepts_passthrough_interrupts()
}

fn log_irq_injection_failure(irq_id: usize, reason: &str) {
    let count = IRQ_INJECTION_DIAGNOSTICS.fetch_add(1, Ordering::Relaxed);
    if count < 8 {
        warn!(
            "AArch64 guest IRQ injection rejected: irq={irq_id}, reason={reason}, sample={}",
            count + 1
        );
    }
}

struct ArmVcpuHostIfImpl;

#[impl_interface]
impl ArmVcpuHostIf for ArmVcpuHostIfImpl {
    fn hardware_inject_virtual_interrupt(vector: u8) {
        let _ = gic::inject_interrupt(vector as usize);
    }

    fn fetch_irq() -> usize {
        gic::fetch_irq()
    }

    fn fetch_fiq() -> usize {
        gic::fetch_fiq()
    }

    fn handle_irq() {
        let start_ns = crate::runtime::rt_trace::now_ns();
        let vector = gic::handle_current_irq();
        crate::runtime::rt_trace::trace_host_irq_handler(
            vector,
            crate::runtime::rt_trace::now_ns().saturating_sub(start_ns),
        );
    }
}

struct ArmVgicHostIfImpl;

#[impl_interface]
impl ArmVgicHostIf for ArmVgicHostIfImpl {
    fn alloc_contiguous_frames(frame_count: usize, frame_align: usize) -> Option<PhysAddr> {
        default_host().alloc_contiguous_frames(frame_count, frame_align)
    }

    fn dealloc_contiguous_frames(start_paddr: PhysAddr, frame_count: usize) {
        default_host().dealloc_contiguous_frames(start_paddr, frame_count);
    }

    fn phys_to_virt(paddr: PhysAddr) -> VirtAddr {
        default_host().phys_to_virt(paddr)
    }

    fn host_cpu_num() -> usize {
        default_host().cpu_count()
    }

    fn current_vcpu_id() -> usize {
        crate::current_vcpu_id().expect("current AArch64 vCPU is not set")
    }

    fn current_time_nanos() -> u64 {
        default_host().monotonic_time().as_nanos() as u64
    }

    fn register_timer(deadline: Duration, callback: Box<dyn FnOnce(Duration) + Send + 'static>) {
        let _ = default_host().register_timer(deadline.as_nanos() as u64, callback);
    }

    fn read_vgicd_iidr() -> u32 {
        gic::read_gicd_iidr()
    }

    fn read_vgicd_typer() -> u32 {
        gic::read_gicd_typer()
    }

    fn get_host_gicd_base() -> PhysAddr {
        gic::host_gicd_base()
    }

    fn get_host_gicr_base() -> PhysAddr {
        gic::host_gicr_base()
    }

    fn hardware_inject_virtual_interrupt(vector: u8) {
        let _ = gic::inject_interrupt(vector as usize);
    }
}
