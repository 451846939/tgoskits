//! AArch64 VM resource creation and initialization.

use alloc::{format, sync::Arc, vec::Vec};

use arm_vcpu::{ArmVcpuCreateConfig, ArmVcpuSetupConfig};
use axdevice_base::DeviceRegistry as _;
use axvm_types::{NestedPagingConfig, VmArchVcpuOps};

use super::{Aarch64Arch, npt};
use crate::{
    AxVmResult,
    architecture::minimum_cpu_capability,
    ax_err, ax_err_type,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Stage2AddressWidth {
    guest_bits: usize,
    host_bits: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Aarch64Stage2Capability {
    levels: usize,
    address_width: Stage2AddressWidth,
}

impl Aarch64Stage2Capability {
    fn nested_paging_config(self, root_paddr: ax_memory_addr::PhysAddr) -> NestedPagingConfig {
        NestedPagingConfig::new(
            root_paddr,
            self.levels,
            self.address_width.guest_bits,
            self.address_width.host_bits,
        )
    }
}

impl Aarch64Arch {
    pub(crate) fn create_vm_resources(config: AxVMConfig) -> AxVmResult<AxVMResources> {
        let placements = config.phys_cpu_ls.get_vcpu_affinities_pcpu_ids();
        let stage2_capability = stage2_capability(&placements)?;
        let page_table = npt::NestedPageTable::new(stage2_capability.levels)?;
        AxVMResources::from_page_table(config, page_table, |root_paddr| {
            Ok(stage2_capability.nested_paging_config(root_paddr))
        })
    }

    pub(crate) fn init_vm(vm: &AxVM, request: VmInitRequest<'_>) -> AxVmResult {
        match request {
            VmInitRequest::Default => {
                let mut factories = default_device_factories()?;
                let emulated_devices = vm.with_config(|config| config.emu_devices().clone());
                let interrupt_fabric = super::irq::configure(
                    &mut factories,
                    vm,
                    vm.interrupt_mode(),
                    &emulated_devices,
                )?;
                init_vm_with(vm, &factories, interrupt_fabric)
            }
            VmInitRequest::Provided {
                factories,
                interrupt_fabric,
            } => init_vm_with(vm, factories, interrupt_fabric),
        }
    }
}

fn init_vm_with(
    vm: &AxVM,
    factories: &axdevice::DeviceFactoryRegistry,
    interrupt_fabric: crate::InterruptFabric,
) -> AxVmResult {
    complete_vm_init(vm, interrupt_fabric, |resources, interrupt_fabric| {
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
        let mut devices = PreparedDevices::build_common(resources, factories, interrupt_fabric)?;
        register_arch_devices(&mut devices.devices)?;
        devices.register_special_devices(vm)?;
        validate_guest_dtb(resources)?;

        let owned_regions = guest_owned_regions(resources);
        map_guest_address_space(vm, resources, devices.devices(), &owned_regions)?;
        vcpus.setup(resources, build_vcpu_setup_config)?;

        Ok(PreparedVm::new(vcpus, devices))
    })
}

fn build_vcpu_setup_config(
    _config: &AxVMConfig,
    _memory_regions: &[crate::vm::VMMemoryRegion],
) -> AxVmResult<<super::AxvmArmVcpu as VmArchVcpuOps>::SetupConfig> {
    Ok(ArmVcpuSetupConfig {
        passthrough_interrupt: false,
        passthrough_timer: false,
    })
}

fn register_arch_devices(devices: &mut axdevice::AxVmDevices) -> AxVmResult {
    register_virtual_timers(devices)
}

fn register_virtual_timers(devices: &mut axdevice::AxVmDevices) -> AxVmResult {
    for device in axdevice::create_vtimer_devices() {
        devices.register(Arc::from(device) as Arc<dyn axdevice_base::Device>)?;
    }
    Ok(())
}

fn stage2_capability(
    vcpu_mappings: &[(usize, Option<usize>, usize)],
) -> AxVmResult<Aarch64Stage2Capability> {
    let cpu_ids = crate::architecture::ops::target_phys_cpu_ids(vcpu_mappings);
    if cpu_ids.is_empty() {
        return ax_err!(
            InvalidInput,
            "AArch64 VM requires at least one target physical CPU"
        );
    }

    let mut cpu_levels = Vec::with_capacity(cpu_ids.len());
    let mut cpu_address_bits = Vec::with_capacity(cpu_ids.len());
    for cpu_id in cpu_ids {
        let target_levels =
            crate::percpu::cpu_max_guest_page_table_levels(cpu_id).ok_or_else(|| {
                ax_err_type!(
                    BadState,
                    format!(
                        "stage-2 page-table capability is unavailable for physical CPU {cpu_id}"
                    )
                )
            })?;
        let address_bits = crate::percpu::cpu_guest_phys_addr_bits(cpu_id).ok_or_else(|| {
            ax_err_type!(
                BadState,
                format!("stage-2 address width is unavailable for physical CPU {cpu_id}")
            )
        })?;

        if target_levels == 0 {
            return ax_err!(
                Unsupported,
                format!("AArch64 nested paging is not enabled on physical CPU {cpu_id}")
            );
        }
        cpu_levels.push(target_levels);
        cpu_address_bits.push(address_bits);
    }
    let levels = minimum_cpu_capability(usize::MAX, cpu_levels);

    let page_table_bits = match levels {
        3 => 39,
        4 => 48,
        _ => {
            return ax_err!(Unsupported, "unsupported AArch64 stage-2 page-table levels");
        }
    };
    let host_bits = minimum_cpu_capability(usize::MAX, cpu_address_bits);
    let address_width = Stage2AddressWidth {
        guest_bits: page_table_bits.min(host_bits),
        host_bits,
    };

    Ok(Aarch64Stage2Capability {
        levels,
        address_width,
    })
}
