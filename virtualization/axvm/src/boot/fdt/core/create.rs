// Copyright 2025 The Axvisor Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use alloc::{format, string::String, vec::Vec};
use core::ptr::NonNull;

use ax_memory_addr::MemoryAddr;
use axvmconfig::{AxVMCrateConfig, EmulatedDeviceConfig, EmulatedDeviceType};
use fdt_edit::{Fdt, Node, NodeId};
use fdt_raw::RegInfo;

use super::tree::{FdtTree, GuestMemorySpec, prop_empty, prop_string, prop_u32_list};
use crate::{
    AxVMRef, AxVmResult, GuestPhysAddr, VMMemoryRegion, ax_err_type,
    boot::images::load_vm_image_from_memory, runtime::IVC_LAYOUT_VERSION,
};

pub fn create_guest_fdt(
    fdt: &Fdt,
    passthrough_device_names: &[String],
    crate_config: &AxVMCrateConfig,
) -> AxVmResult<Vec<u8>> {
    let phys_cpu_ids = crate_config
        .base
        .phys_cpu_ids
        .as_deref()
        .ok_or_else(|| ax_err_type!(InvalidInput, "phys_cpu_ids is missing"))?;

    let mut guest_tree = FdtTree::clone_filtered(fdt, |node_id, path, node| {
        should_keep_generated_node(
            fdt,
            node_id,
            path,
            node,
            passthrough_device_names,
            phys_cpu_ids,
        )
    })?;
    guest_tree.add_ivc_channel_nodes(&crate_config.devices.emu_devices)?;
    guest_tree.add_vpci_host_nodes(&crate_config.devices.emu_devices)?;
    Ok(guest_tree.finish())
}

fn should_keep_generated_node(
    fdt: &Fdt,
    node_id: NodeId,
    node_path: &str,
    node: &Node,
    passthrough_device_names: &[String],
    phys_cpu_ids: &[usize],
) -> bool {
    if node.name().starts_with("memory") {
        return false;
    }

    if node_path == "/cpus" || node_path.starts_with("/cpus/cpu-map") {
        return true;
    }

    if node_path.starts_with("/cpus/cpu@") {
        return need_cpu_node(phys_cpu_ids, fdt, node_id, node_path);
    }

    passthrough_device_names
        .iter()
        .any(|device_path| device_path == node_path)
        || is_descendant_of_passthrough_device(node_path, passthrough_device_names)
        || is_ancestor_of_passthrough_device(node_path, passthrough_device_names)
}

fn is_descendant_of_passthrough_device(
    node_path: &str,
    passthrough_device_names: &[String],
) -> bool {
    passthrough_device_names.iter().any(|passthrough_path| {
        node_path
            .strip_prefix(passthrough_path)
            .is_some_and(|suffix| suffix.starts_with('/'))
    })
}

fn is_ancestor_of_passthrough_device(node_path: &str, passthrough_device_names: &[String]) -> bool {
    passthrough_device_names.iter().any(|passthrough_path| {
        passthrough_path
            .strip_prefix(node_path)
            .is_some_and(|suffix| suffix.starts_with('/'))
            || node_path == "/"
    })
}

fn cpu_node_id(node_path: &str) -> Option<usize> {
    node_path
        .strip_prefix("/cpus/cpu@")
        .and_then(|rest| rest.split('/').next())
        .and_then(|id| usize::from_str_radix(id, 16).ok())
}

fn cpu_reg_address(fdt: &Fdt, node_id: NodeId) -> Option<usize> {
    fdt.view_typed(node_id)
        .and_then(|node| node.regs().first().map(|reg| reg.address as usize))
}

pub(crate) fn need_cpu_node(
    phys_cpu_ids: &[usize],
    fdt: &Fdt,
    node_id: NodeId,
    node_path: &str,
) -> bool {
    if !node_path.starts_with("/cpus/cpu@") {
        return true;
    }

    if let Some(cpu_id) = cpu_node_id(node_path) {
        return phys_cpu_ids.contains(&cpu_id);
    }

    cpu_reg_address(fdt, node_id).is_some_and(|cpu_address| {
        debug!("Checking CPU node {node_path} with address 0x{cpu_address:x}");
        phys_cpu_ids.contains(&cpu_address)
    })
}

fn guest_memory_specs(
    new_memory: &[VMMemoryRegion],
    crate_config: &AxVMCrateConfig,
) -> Vec<GuestMemorySpec> {
    let configured_region_count = if crate_config.kernel.configured_memory_region_count == 0 {
        crate_config.kernel.memory_regions.len()
    } else {
        crate_config
            .kernel
            .configured_memory_region_count
            .min(crate_config.kernel.memory_regions.len())
    };

    if new_memory.len() != crate_config.kernel.memory_regions.len() {
        warn!(
            "VM memory region count {} does not match config region count {}; filtering /memory \
             by zipped order",
            new_memory.len(),
            crate_config.kernel.memory_regions.len()
        );
    }

    new_memory
        .iter()
        .take(configured_region_count)
        .zip(
            crate_config
                .kernel
                .memory_regions
                .iter()
                .take(configured_region_count),
        )
        .map(|(mem, _cfg)| GuestMemorySpec::new(mem.gpa.as_usize() as u64, mem.size() as u64))
        .collect()
}

#[cfg(test)]
fn initrd_range_from_image_config(
    ramdisk: Option<&crate::config::RamdiskInfo>,
) -> Option<(u64, u64)> {
    let ramdisk = ramdisk?;
    let start = ramdisk.load_gpa.as_usize() as u64;
    let size = ramdisk.size? as u64;
    Some((start, start.saturating_add(size)))
}

pub fn update_fdt(
    fdt_src: NonNull<u8>,
    dtb_size: usize,
    vm: AxVMRef,
    crate_config: &AxVMCrateConfig,
) -> AxVmResult {
    let patch_runtime = super::selected_guest_fdt_policy().patch_runtime;
    // SAFETY: `fdt_src` originates from `GuestDtbImage::as_bytes`, and the
    // caller supplies the exact slice length while the image remains borrowed.
    let fdt_bytes = unsafe { core::slice::from_raw_parts(fdt_src.as_ptr(), dtb_size) };
    let new_fdt_bytes = patch_runtime(fdt_bytes, &vm, crate_config)?;

    load_patched_fdt(vm, new_fdt_bytes)
}

fn load_patched_fdt(vm: AxVMRef, new_fdt_bytes: Vec<u8>) -> AxVmResult {
    let dest_addr = calculate_dtb_load_addr(vm.clone(), new_fdt_bytes.len())?;
    debug!(
        "New FDT will be loaded at {:x}, size: 0x{:x}",
        dest_addr,
        new_fdt_bytes.len()
    );
    load_vm_image_from_memory(&new_fdt_bytes, dest_addr, vm.clone())?;
    vm.set_guest_device_tree(dest_addr, new_fdt_bytes)
}

pub fn patch_guest_fdt_for_runtime(
    fdt_bytes: &[u8],
    memory_regions: &[VMMemoryRegion],
    crate_config: &AxVMCrateConfig,
    initrd_start_size: Option<(u64, u64)>,
    create_chosen: bool,
) -> AxVmResult<Vec<u8>> {
    let mut tree = FdtTree::from_bytes(fdt_bytes)?;
    let memory_specs = guest_memory_specs(memory_regions, crate_config);
    tree.rebuild_memory_nodes(&memory_specs)?;
    tree.add_ivc_channel_nodes(&crate_config.devices.emu_devices)?;
    tree.add_vpci_host_nodes(&crate_config.devices.emu_devices)?;
    if create_chosen
        || initrd_start_size.is_some()
        || tree.inner().get_by_path_id("/chosen").is_some()
    {
        tree.patch_chosen(initrd_start_size)?;
    }
    Ok(tree.finish())
}

impl FdtTree {
    fn next_phandle(&self) -> u32 {
        const AXIVC_PHANDLE_BASE: u32 = 0xa11c_0000;

        self.inner()
            .iter_node_ids()
            .filter_map(|id| self.inner().node(id))
            .flat_map(|node| {
                [
                    node.get_property("phandle").and_then(|prop| prop.get_u32()),
                    node.get_property("linux,phandle")
                        .and_then(|prop| prop.get_u32()),
                ]
            })
            .flatten()
            .filter(|phandle| *phandle >= AXIVC_PHANDLE_BASE)
            .max()
            .and_then(|phandle| phandle.checked_add(1))
            .unwrap_or(AXIVC_PHANDLE_BASE)
    }

    fn root_cells(&self, property: &str, fallback: u32) -> u32 {
        self.inner()
            .node(self.inner().root_id())
            .and_then(|node| node.get_property(property))
            .and_then(|prop| prop.get_u32())
            .unwrap_or(fallback)
    }

    fn add_ivc_channel_nodes(&mut self, devices: &[EmulatedDeviceConfig]) -> AxVmResult {
        for device in devices
            .iter()
            .filter(|device| device.emu_type == EmulatedDeviceType::IVCChannel)
        {
            self.add_ivc_channel_node(device)?;
        }
        Ok(())
    }

    fn add_vpci_host_nodes(&mut self, devices: &[EmulatedDeviceConfig]) -> AxVmResult {
        for device in devices
            .iter()
            .filter(|device| device.emu_type == EmulatedDeviceType::VirtualPciHost)
        {
            self.add_vpci_host_node(device)?;
        }
        Ok(())
    }

    fn add_vpci_host_node(&mut self, device: &EmulatedDeviceConfig) -> AxVmResult {
        let node_path = format!("/pcie@{:x}", device.base_gpa);
        let node_id = self.ensure_path(&node_path)?;
        let bus = device.cfg_list.first().copied().unwrap_or(0) as u32;
        let mem_base = device
            .cfg_list
            .get(5)
            .copied()
            .map(|base| base as u64)
            .unwrap_or_else(|| {
                align_up_u64(device.base_gpa as u64 + device.length as u64, 0x10_0000)
            });
        let mem_size = device
            .cfg_list
            .get(6)
            .copied()
            .map(|size| size as u64)
            .unwrap_or(0x10_0000);

        info!("Adding guest virtual PCI host FDT node {node_path}");
        self.set_property(node_id, prop_string("compatible", "pci-host-ecam-generic"))?;
        self.set_property(node_id, prop_string("device_type", "pci"))?;
        self.set_property(node_id, prop_string("status", "okay"))?;
        self.set_property(node_id, prop_u32_list("#address-cells", &[3]))?;
        self.set_property(node_id, prop_u32_list("#size-cells", &[2]))?;
        self.set_property(node_id, prop_u32_list("#interrupt-cells", &[1]))?;
        self.set_property(node_id, prop_u32_list("bus-range", &[bus, bus]))?;
        self.set_property(node_id, prop_empty("dma-coherent"))?;
        self.set_property(node_id, self.vpci_ranges_property(mem_base, mem_size))?;

        self.inner_mut()
            .view_typed_mut(node_id)
            .ok_or_else(|| ax_err_type!(InvalidData, "new virtual PCI host node is missing"))?
            .set_regs(&[RegInfo::new(
                device.base_gpa as u64,
                Some(device.length as u64),
            )]);
        Ok(())
    }

    fn vpci_ranges_property(&self, mem_base: u64, mem_size: u64) -> fdt_edit::Property {
        const PCI_RANGE_MEM32: u32 = 0x0200_0000;

        let mut cells = Vec::new();
        cells.push(PCI_RANGE_MEM32);
        cells.push((mem_base >> 32) as u32);
        cells.push(mem_base as u32);
        push_u64_cells(&mut cells, mem_base, self.root_cells("#address-cells", 2));
        push_u64_cells(&mut cells, mem_size, 2);
        prop_u32_list("ranges", &cells)
    }

    fn add_ivc_channel_node(&mut self, device: &EmulatedDeviceConfig) -> AxVmResult {
        let reserved_phandle = self.add_ivc_reserved_memory_node(device)?;
        let node_id = self.ensure_path(&format!("/ivc-channel@{:x}", device.base_gpa))?;
        info!(
            "Adding guest IVC channel FDT node /ivc-channel@{:x}",
            device.base_gpa
        );
        self.set_property(node_id, prop_string("compatible", "axvisor,ivc-channel"))?;
        self.set_property(node_id, prop_string("status", "okay"))?;
        self.set_property(
            node_id,
            prop_u32_list("axvisor,ivc-version", &[IVC_LAYOUT_VERSION as u32]),
        )?;
        self.set_property(node_id, prop_u32_list("memory-region", &[reserved_phandle]))?;
        self.set_property(node_id, prop_empty("dma-coherent"))?;

        if let Some(notify_irq) = device
            .cfg_list
            .first()
            .and_then(|irq| u32::try_from(*irq).ok())
        {
            self.set_property(node_id, prop_u32_list("axvisor,notify-irq", &[notify_irq]))?;
        }

        self.inner_mut()
            .view_typed_mut(node_id)
            .ok_or_else(|| ax_err_type!(InvalidData, "new IVC channel node is missing"))?
            .set_regs(&[RegInfo::new(
                device.base_gpa as u64,
                Some(device.length as u64),
            )]);
        Ok(())
    }

    fn add_ivc_reserved_memory_node(&mut self, device: &EmulatedDeviceConfig) -> AxVmResult<u32> {
        let reserved_id = self.ensure_path("/reserved-memory")?;
        let address_cells = self.root_cells("#address-cells", 2);
        let size_cells = self.root_cells("#size-cells", 2);
        self.set_property(
            reserved_id,
            prop_u32_list("#address-cells", &[address_cells]),
        )?;
        self.set_property(reserved_id, prop_u32_list("#size-cells", &[size_cells]))?;
        self.set_property(reserved_id, prop_empty("ranges"))?;

        let node_path = format!("/reserved-memory/axivc-shm@{:x}", device.base_gpa);
        let node_id = self.ensure_path(&node_path)?;
        let phandle = self
            .inner()
            .node(node_id)
            .and_then(|node| node.get_property("phandle"))
            .and_then(|prop| prop.get_u32())
            .unwrap_or_else(|| self.next_phandle());

        info!("Adding guest IVC reserved-memory FDT node {node_path}");
        self.set_property(node_id, prop_string("compatible", "shared-dma-pool"))?;
        self.set_property(node_id, prop_u32_list("phandle", &[phandle]))?;
        self.set_property(node_id, prop_empty("no-map"))?;
        self.inner_mut()
            .view_typed_mut(node_id)
            .ok_or_else(|| ax_err_type!(InvalidData, "new IVC reserved-memory node is missing"))?
            .set_regs(&[RegInfo::new(
                device.base_gpa as u64,
                Some(device.length as u64),
            )]);

        Ok(phandle)
    }
}

fn align_up_u64(value: u64, align: u64) -> u64 {
    debug_assert!(align.is_power_of_two());
    (value + align - 1) & !(align - 1)
}

fn push_u64_cells(cells: &mut Vec<u32>, value: u64, cell_count: u32) {
    match cell_count {
        0 => {}
        1 => cells.push(value as u32),
        _ => {
            cells.push((value >> 32) as u32);
            cells.push(value as u32);
            for _ in 2..cell_count {
                cells.push(0);
            }
        }
    }
}

pub(crate) fn calculate_dtb_load_addr(vm: AxVMRef, fdt_size: usize) -> AxVmResult<GuestPhysAddr> {
    const MB: usize = 1024 * 1024;

    let main_memory =
        vm.memory_regions().first().cloned().ok_or_else(|| {
            ax_err_type!(InvalidInput, "VM has no memory region for DTB placement")
        })?;

    let dtb_addr = vm.with_config(|config| {
        let use_configured_dtb_addr =
            config.image_config.dtb_load_gpa.is_some() && !main_memory.is_identical();

        let dtb_addr = if let Some(configured) = config
            .image_config
            .dtb_load_gpa
            .filter(|_| use_configured_dtb_addr)
        {
            configured
        } else {
            let main_memory_size = main_memory.size().min(512 * MB);
            let addr = (main_memory.gpa + main_memory_size - fdt_size).align_down(2 * MB);
            if fdt_size > main_memory_size {
                error!("DTB size is larger than available memory");
            }
            addr
        };
        config.image_config.dtb_load_gpa = Some(dtb_addr);
        dtb_addr
    });

    Ok(dtb_addr)
}

#[cfg(test)]
mod tests {
    use axvmconfig::AxVMCrateConfig;
    use fdt_edit::{Fdt, Node, Property};
    use fdt_raw::RegInfo;

    use super::{
        super::tree::sanitize_bootargs, cpu_node_id, initrd_range_from_image_config, need_cpu_node,
    };
    use crate::{GuestPhysAddr, config::RamdiskInfo};

    fn prop_u32(name: &str, value: u32) -> Property {
        let mut prop = Property::new(name, alloc::vec![]);
        prop.set_u32_ls(&[value]);
        prop
    }

    fn test_fdt(dts: &str) -> Fdt {
        let mut fdt = Fdt::new();
        let root = fdt.root_id();
        let cpus = fdt.add_node(root, Node::new("cpus"));
        fdt.node_mut(cpus)
            .unwrap()
            .set_property(prop_u32("#address-cells", 2));
        fdt.node_mut(cpus)
            .unwrap()
            .set_property(prop_u32("#size-cells", 0));

        for line in dts.lines().map(str::trim).filter(|line| !line.is_empty()) {
            let (name, reg) = line.split_once('=').unwrap();
            let node = fdt.add_node(cpus, Node::new(name));
            let reg = usize::from_str_radix(reg, 16).unwrap();
            fdt.view_typed_mut(node)
                .unwrap()
                .set_regs(&[RegInfo::new(reg as u64, None)]);
        }

        fdt
    }

    #[test]
    fn cpu_node_selection_uses_node_id_when_reg_differs() {
        let fdt = test_fdt("cpu@0=200\ncpu@100=0\ncpu@101=100");
        let selected: alloc::vec::Vec<_> = fdt
            .iter_node_ids()
            .map(|id| (id, fdt.path_of(id)))
            .filter(|(_, path)| path.starts_with("/cpus/cpu@"))
            .filter_map(|(id, path)| need_cpu_node(&[0x100], &fdt, id, &path).then_some(path))
            .collect();

        assert_eq!(selected, ["/cpus/cpu@100"]);
    }

    #[test]
    fn cpu_node_id_parses_hex_unit_address() {
        assert_eq!(cpu_node_id("/cpus/cpu@100"), Some(0x100));
    }

    #[test]
    fn initrd_range_requires_both_address_and_size() {
        assert_eq!(
            initrd_range_from_image_config(Some(&RamdiskInfo {
                load_gpa: GuestPhysAddr::from(0xa000_0000usize),
                size: None,
            })),
            None
        );
        assert_eq!(
            initrd_range_from_image_config(Some(&RamdiskInfo {
                load_gpa: GuestPhysAddr::from(0xa000_0000usize),
                size: Some(0x1234),
            })),
            Some((0xa000_0000, 0xa000_1234))
        );
    }

    #[test]
    fn sanitize_bootargs_enables_auto_repair_for_block_roots() {
        let bootargs = "root=/dev/mmcblk0p2 rw console=ttyS2,1500000 rootwait rootfstype=ext4";

        assert_eq!(
            sanitize_bootargs(bootargs),
            "root=/dev/mmcblk0p2 rw console=ttyS2,1500000 rootwait rootfstype=ext4 fsck.repair=yes"
        );
    }

    #[test]
    fn sanitize_bootargs_preserves_existing_fsck_policy() {
        let bootargs =
            "root=/dev/mmcblk0p2 ro rootwait rootfstype=ext4 fsckfix rdinit=/init root=/dev/ram0";

        assert_eq!(
            sanitize_bootargs(bootargs),
            "root=/dev/mmcblk0p2 rw rootwait rootfstype=ext4 fsckfix"
        );
    }

    #[test]
    fn runtime_patch_can_leave_missing_chosen_for_host_copy() {
        let fdt = Fdt::new();
        let dtb = fdt.encode().as_ref().to_vec();
        let cfg = AxVMCrateConfig::default();

        let patched = super::patch_guest_fdt_for_runtime(&dtb, &[], &cfg, None, false).unwrap();
        let reparsed = Fdt::from_bytes(&patched).unwrap();

        assert!(reparsed.get_by_path_id("/chosen").is_none());

        let patched = super::patch_guest_fdt_for_runtime(&dtb, &[], &cfg, None, true).unwrap();
        let reparsed = Fdt::from_bytes(&patched).unwrap();

        assert!(reparsed.get_by_path_id("/chosen").is_some());
    }

    #[test]
    fn runtime_patch_adds_ivc_channel_node() {
        let fdt = Fdt::new();
        let dtb = fdt.encode().as_ref().to_vec();
        let cfg = AxVMCrateConfig {
            devices: axvmconfig::VMDevicesConfig {
                emu_devices: alloc::vec![axvmconfig::EmulatedDeviceConfig {
                    name: "ivc-channel".into(),
                    base_gpa: 0xbff0_0000,
                    length: 0x1_0000,
                    irq_id: 0,
                    emu_type: axvmconfig::EmulatedDeviceType::IVCChannel,
                    cfg_list: alloc::vec![60],
                }],
                ..Default::default()
            },
            ..Default::default()
        };

        let patched = super::patch_guest_fdt_for_runtime(&dtb, &[], &cfg, None, false).unwrap();
        let reparsed = Fdt::from_bytes(&patched).unwrap();
        let node_id = reparsed.get_by_path_id("/ivc-channel@bff00000").unwrap();
        let node = reparsed.node(node_id).unwrap();
        let typed_node = reparsed.view_typed(node_id).unwrap();

        assert_eq!(
            node.get_property("compatible").unwrap().as_str(),
            Some("axvisor,ivc-channel")
        );
        assert_eq!(typed_node.regs()[0].address, 0xbff0_0000);
        assert_eq!(typed_node.regs()[0].size, Some(0x1_0000));
        assert_eq!(
            node.get_property("axvisor,notify-irq").unwrap().get_u32(),
            Some(60)
        );

        let shm_id = reparsed
            .get_by_path_id("/reserved-memory/axivc-shm@bff00000")
            .unwrap();
        let shm_node = reparsed.node(shm_id).unwrap();
        let shm_typed_node = reparsed.view_typed(shm_id).unwrap();
        let shm_phandle = shm_node.get_property("phandle").unwrap().get_u32();

        assert_eq!(
            shm_node.get_property("compatible").unwrap().as_str(),
            Some("shared-dma-pool")
        );
        assert!(shm_node.get_property("no-map").is_some());
        assert_eq!(shm_typed_node.regs()[0].address, 0xbff0_0000);
        assert_eq!(shm_typed_node.regs()[0].size, Some(0x1_0000));
        assert_eq!(
            node.get_property("memory-region").unwrap().get_u32(),
            shm_phandle
        );
        assert!(node.get_property("dma-coherent").is_some());
    }

    #[test]
    fn generated_fdt_filters_cpu_nodes_by_unit_address() {
        let fdt = test_fdt("cpu@0=200\ncpu@100=0\ncpu@101=100");
        let cfg = AxVMCrateConfig {
            base: axvmconfig::VMBaseConfig {
                phys_cpu_ids: Some(alloc::vec![0x100]),
                ..Default::default()
            },
            ..Default::default()
        };
        let dtb = super::create_guest_fdt(&fdt, &[], &cfg).unwrap();
        let reparsed = Fdt::from_bytes(&dtb).unwrap();

        assert!(reparsed.get_by_path_id("/cpus/cpu@100").is_some());
        assert!(reparsed.get_by_path_id("/cpus/cpu@0").is_none());
        assert!(reparsed.get_by_path_id("/cpus/cpu@101").is_none());
    }

    #[test]
    fn generated_fdt_adds_ivc_channel_node() {
        let fdt = test_fdt("cpu@0=0");
        let cfg = AxVMCrateConfig {
            base: axvmconfig::VMBaseConfig {
                phys_cpu_ids: Some(alloc::vec![0]),
                ..Default::default()
            },
            devices: axvmconfig::VMDevicesConfig {
                emu_devices: alloc::vec![axvmconfig::EmulatedDeviceConfig {
                    name: "ivc-channel".into(),
                    base_gpa: 0xbff0_0000,
                    length: 0x1_0000,
                    irq_id: 0,
                    emu_type: axvmconfig::EmulatedDeviceType::IVCChannel,
                    cfg_list: alloc::vec![60],
                }],
                ..Default::default()
            },
            ..Default::default()
        };
        let dtb = super::create_guest_fdt(&fdt, &[], &cfg).unwrap();
        let reparsed = Fdt::from_bytes(&dtb).unwrap();
        let node_id = reparsed.get_by_path_id("/ivc-channel@bff00000").unwrap();
        let node = reparsed.node(node_id).unwrap();

        assert_eq!(
            node.get_property("compatible").unwrap().as_str(),
            Some("axvisor,ivc-channel")
        );
        assert!(
            reparsed
                .get_by_path_id("/reserved-memory/axivc-shm@bff00000")
                .is_some()
        );
        assert!(node.get_property("memory-region").is_some());
    }

    #[test]
    fn runtime_patch_adds_virtual_pci_host_node() {
        let mut fdt = Fdt::new();
        let root = fdt.root_id();
        fdt.node_mut(root)
            .unwrap()
            .set_property(prop_u32("#address-cells", 2));
        fdt.node_mut(root)
            .unwrap()
            .set_property(prop_u32("#size-cells", 2));
        let dtb = fdt.encode().as_ref().to_vec();
        let cfg = AxVMCrateConfig {
            devices: axvmconfig::VMDevicesConfig {
                emu_devices: alloc::vec![axvmconfig::EmulatedDeviceConfig {
                    name: "vpci-host".into(),
                    base_gpa: 0x5000_0000,
                    length: 0x10_0000,
                    irq_id: 0,
                    emu_type: axvmconfig::EmulatedDeviceType::VirtualPciHost,
                    cfg_list: alloc::vec![0, 5, 0, 0xaaaa, 0x0001],
                }],
                ..Default::default()
            },
            ..Default::default()
        };

        let patched = super::patch_guest_fdt_for_runtime(&dtb, &[], &cfg, None, false).unwrap();
        let reparsed = Fdt::from_bytes(&patched).unwrap();
        let node_id = reparsed.get_by_path_id("/pcie@50000000").unwrap();
        let node = reparsed.node(node_id).unwrap();
        let typed_node = reparsed.view_typed(node_id).unwrap();

        assert_eq!(
            node.get_property("compatible").unwrap().as_str(),
            Some("pci-host-ecam-generic")
        );
        assert_eq!(
            node.get_property("device_type").unwrap().as_str(),
            Some("pci")
        );
        assert_eq!(
            node.get_property("#address-cells").unwrap().get_u32(),
            Some(3)
        );
        assert_eq!(node.get_property("#size-cells").unwrap().get_u32(), Some(2));
        assert!(node.get_property("bus-range").is_some());
        assert!(node.get_property("ranges").is_some());
        assert_eq!(typed_node.regs()[0].address, 0x5000_0000);
        assert_eq!(typed_node.regs()[0].size, Some(0x10_0000));
    }
}
