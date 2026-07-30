//! AArch64 VM resource creation and initialization.

use alloc::sync::Arc;

use arm_vcpu::{ArmVcpuCreateConfig, ArmVcpuSetupConfig};
use arm_vgic::PpiId;
use axdevice::DeviceFactoryRegistry;
use axvm_types::{EmulatedDeviceConfig, EmulatedDeviceType, NestedPagingConfig, VmArchVcpuOps};

use super::{
    Aarch64Arch, npt,
    vgic::{self, Aarch64VgicRuntime, GUEST_PHYSICAL_TIMER_PPI},
};
use crate::{
    AxVmResult, ax_err,
    config::AxVMConfig,
    vm::{
        AxVM, AxVMResources,
        prepare::{
            PreparedVm, VmInitRequest,
            address_space::{guest_owned_regions, map_guest_address_space},
            complete_vm_init, default_device_factories,
            devices::PreparedDevices,
            validate_guest_dtb,
            vcpus::{PreparedVcpus, vcpu_placements},
        },
    },
};

impl Aarch64Arch {
    pub(crate) fn create_vm_resources(config: AxVMConfig) -> AxVmResult<AxVMResources> {
        let placements = config.phys_cpu_ls.get_vcpu_affinities_pcpu_ids();
        let levels = guest_page_table_levels(&placements)?;
        let page_table = npt::NestedPageTable::new(levels)?;
        AxVMResources::from_page_table(config, page_table, |root_paddr| {
            nested_paging_config(root_paddr, levels, &placements)
        })
    }

    pub(crate) fn init_vm(vm: &AxVM, request: VmInitRequest<'_>) -> AxVmResult {
        match request {
            VmInitRequest::Default => {
                let mut factories = default_device_factories(vm)?;
                let runtime = register_device_factories(vm, &mut factories)?;
                init_vm_with(vm, &factories, runtime)
            }
            VmInitRequest::Provided { factories } => {
                let runtime = register_device_factories(vm, factories)?;
                init_vm_with(vm, factories, runtime)
            }
        }
    }
}

fn register_device_factories(
    vm: &AxVM,
    factories: &mut DeviceFactoryRegistry,
) -> AxVmResult<Arc<Aarch64VgicRuntime>> {
    let runtime = vgic::register_device_factories(vm, factories)?;
    let vcpu_count =
        vm.with_config(|config| config.phys_cpu_ls.get_vcpu_affinities_pcpu_ids().len());
    let timer_ppi = PpiId::new(GUEST_PHYSICAL_TIMER_PPI)
        .map_err(|error| crate::AxVmError::interrupt("validate timer PPI", error))?;
    factories.register(Arc::new(super::vtimer::Aarch64VtimerFactory::new(
        runtime.core().clone(),
        vcpu_count,
        timer_ppi,
    )?))?;
    Ok(runtime)
}

fn init_vm_with(
    vm: &AxVM,
    factories: &DeviceFactoryRegistry,
    vgic_runtime: Arc<Aarch64VgicRuntime>,
) -> AxVmResult {
    let interrupt_controller: Arc<dyn axdevice_base::VirtualInterruptController> =
        vgic_runtime.core().clone();
    complete_vm_init(
        vm,
        interrupt_controller,
        |resources, interrupt_controller| {
            let placements = vcpu_placements(resources);
            let dtb_addr = resources
                .config()
                .image_config()
                .dtb_load_gpa
                .unwrap_or_default();
            let vcpus = PreparedVcpus::create(vm.id(), &placements, |placement| {
                Ok(ArmVcpuCreateConfig {
                    mpidr_el1: placement.phys_cpu_id as _,
                    dtb_addr: dtb_addr.as_usize(),
                })
            })?;
            for vcpu in &vcpus {
                let binding = vgic_runtime.attach_vcpu(vcpu.id()).map_err(|error| {
                    crate::AxVmError::interrupt("attach vCPU to virtual GIC", error)
                })?;
                vcpu.get_arch_vcpu()
                    .attach_vgic(vgic_runtime.core().clone(), binding)?;
            }

            let devices = PreparedDevices::build_common_with_extra(
                resources,
                factories,
                interrupt_controller,
                &arch_extra_device_configs(),
                vm.device_access_ports(),
            )?;
            validate_guest_dtb(resources)?;

            let owned_regions = guest_owned_regions(resources);
            map_guest_address_space(vm, resources, devices.devices(), &owned_regions)?;
            vcpus.setup(resources, build_vcpu_setup_config)?;

            Ok(PreparedVm::new(vcpus, devices))
        },
    )
}

fn build_vcpu_setup_config(
    _config: &AxVMConfig,
    _memory_regions: &[crate::vm::VMMemoryRegion],
) -> AxVmResult<<super::AxvmArmVcpu as VmArchVcpuOps>::SetupConfig> {
    Ok(ArmVcpuSetupConfig)
}

fn arch_extra_device_configs() -> [EmulatedDeviceConfig; 1] {
    [EmulatedDeviceConfig {
        name: "aarch64-vtimer".into(),
        base_gpa: 0,
        length: 0,
        irq_id: 0,
        emu_type: EmulatedDeviceType::Aarch64Vtimer,
        cfg_list: alloc::vec![],
    }]
}

fn guest_page_table_levels(vcpu_mappings: &[(usize, Option<usize>, usize)]) -> AxVmResult<usize> {
    let selected = crate::architecture::minimum_target_cpu_capability(
        arm_vcpu::max_guest_page_table_levels(),
        vcpu_mappings,
        |cpu_id| {
            crate::percpu::cpu_max_guest_page_table_levels(cpu_id)
                .unwrap_or_else(arm_vcpu::max_guest_page_table_levels)
        },
    );
    match selected {
        0 => ax_err!(
            Unsupported,
            "AArch64 nested paging is not enabled on target CPU"
        ),
        3 | 4 => Ok(selected),
        _ => ax_err!(Unsupported, "unsupported AArch64 stage-2 page-table levels"),
    }
}

fn nested_paging_config(
    root_paddr: ax_memory_addr::PhysAddr,
    levels: usize,
    vcpu_mappings: &[(usize, Option<usize>, usize)],
) -> AxVmResult<NestedPagingConfig> {
    let pa_bits = crate::architecture::minimum_target_cpu_capability(
        arm_vcpu::pa_bits(),
        vcpu_mappings,
        |cpu_id| crate::percpu::cpu_guest_phys_addr_bits(cpu_id).unwrap_or_else(arm_vcpu::pa_bits),
    );

    let gpa_bits = match levels {
        3 => 39,
        4 => 48,
        _ => return ax_err!(InvalidInput, "unsupported AArch64 stage-2 levels"),
    };
    Ok(NestedPagingConfig::new(
        root_paddr, levels, gpa_bits, pa_bits,
    ))
}
