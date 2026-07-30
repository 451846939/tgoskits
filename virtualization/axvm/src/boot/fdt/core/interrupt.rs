//! Machine-owned interrupt-controller description for guest device trees.

use axvm_types::EmulatedDeviceType;
use fdt_edit::{Fdt, NodeId, Property};
use fdt_raw::RegInfo;

use super::tree::FdtTree;
use crate::{
    AxVmResult, ax_err_type,
    machine::{GuestGicProfile, GuestPlicProfile},
};

/// Rewrites the interrupt-controller resources to match the VM-owned controller.
pub(crate) fn install_machine_interrupt_controller(
    tree: &mut FdtTree,
    cpu_num: usize,
    gic_profile: Option<&GuestGicProfile>,
    plic_profile: Option<&GuestPlicProfile>,
) -> AxVmResult {
    if let Some(profile) = plic_profile {
        return install_plic_registers(tree, profile);
    }

    let fallback;
    let profile = match gic_profile {
        Some(profile) => profile,
        None => {
            let machine = crate::machine::current_machine_profile(cpu_num);
            let distributor = machine
                .emulated_devices
                .iter()
                .find(|device| device.emu_type == EmulatedDeviceType::InterruptController);
            let redistributor = machine
                .emulated_devices
                .iter()
                .find(|device| device.emu_type == EmulatedDeviceType::GPPTRedistributor);
            let (Some(distributor), Some(redistributor)) = (distributor, redistributor) else {
                return Ok(());
            };
            fallback = GuestGicProfile {
                node_path: alloc::string::String::new(),
                node_phandle: None,
                distributor_base: distributor.base_gpa,
                distributor_length: distributor.length,
                redistributor_base: redistributor.base_gpa,
                redistributor_length: redistributor.length,
            };
            &fallback
        }
    };
    install_gic_registers(tree, profile)
}

/// Reads the host PLIC register window and firmware identity.
pub(crate) fn host_plic_profile(fdt: &Fdt) -> AxVmResult<Option<GuestPlicProfile>> {
    let Some(controller) = find_plic_in_fdt(fdt) else {
        return Ok(None);
    };
    let view = fdt
        .view_typed(controller)
        .ok_or_else(|| ax_err_type!(InvalidData, "host PLIC node is missing"))?;
    let reg = view
        .regs()
        .into_iter()
        .next()
        .ok_or_else(|| ax_err_type!(InvalidData, "host PLIC node has no register range"))?;
    let (base, length) = checked_plic_reg(&reg)?;
    let node = view.as_node();

    Ok(Some(GuestPlicProfile {
        node_path: fdt.path_of(controller),
        node_phandle: node
            .get_property("phandle")
            .or_else(|| node.get_property("linux,phandle"))
            .and_then(Property::get_u32),
        base,
        length,
    }))
}

/// Reads the host GICv3 register windows and firmware identity.
pub(crate) fn host_gic_profile(fdt: &Fdt) -> AxVmResult<Option<GuestGicProfile>> {
    let Some(controller) = fdt.iter_node_ids().find(|node_id| {
        fdt.node(*node_id).is_some_and(|node| {
            node.get_property("interrupt-controller").is_some()
                && node
                    .compatibles()
                    .any(|compatible| compatible == "arm,gic-v3")
        })
    }) else {
        return Ok(None);
    };
    let view = fdt
        .view_typed(controller)
        .ok_or_else(|| ax_err_type!(InvalidData, "host GICv3 node is missing"))?;
    let regs = view.regs();
    if regs.len() < 2 {
        return Err(ax_err_type!(
            InvalidData,
            "host GICv3 node must provide distributor and redistributor ranges"
        ));
    }
    let (distributor_base, distributor_length) = checked_reg(&regs[0], "distributor")?;
    let (redistributor_base, redistributor_length) = checked_reg(&regs[1], "redistributor")?;
    let node = view.as_node();

    Ok(Some(GuestGicProfile {
        node_path: fdt.path_of(controller),
        node_phandle: node
            .get_property("phandle")
            .or_else(|| node.get_property("linux,phandle"))
            .and_then(Property::get_u32),
        distributor_base,
        distributor_length,
        redistributor_base,
        redistributor_length,
    }))
}

fn checked_reg(reg: &fdt_edit::RegFixed, name: &str) -> AxVmResult<(usize, usize)> {
    let base = usize::try_from(reg.address).map_err(|_| {
        ax_err_type!(
            InvalidData,
            alloc::format!("host GIC {name} address does not fit usize")
        )
    })?;
    let length = reg
        .size
        .ok_or_else(|| {
            ax_err_type!(
                InvalidData,
                alloc::format!("host GIC {name} range has no size")
            )
        })
        .and_then(|length| {
            usize::try_from(length).map_err(|_| {
                ax_err_type!(
                    InvalidData,
                    alloc::format!("host GIC {name} range size does not fit usize")
                )
            })
        })?;
    if length == 0 {
        return Err(ax_err_type!(
            InvalidData,
            alloc::format!("host GIC {name} range is empty")
        ));
    }
    Ok((base, length))
}

fn checked_plic_reg(reg: &fdt_edit::RegFixed) -> AxVmResult<(usize, usize)> {
    let base = usize::try_from(reg.address)
        .map_err(|_| ax_err_type!(InvalidData, "host PLIC address does not fit usize"))?;
    let length = reg
        .size
        .ok_or_else(|| ax_err_type!(InvalidData, "host PLIC range has no size"))
        .and_then(|length| {
            usize::try_from(length)
                .map_err(|_| ax_err_type!(InvalidData, "host PLIC range size does not fit usize"))
        })?;
    if length == 0 {
        return Err(ax_err_type!(InvalidData, "host PLIC range is empty"));
    }
    Ok((base, length))
}

fn install_gic_registers(tree: &mut FdtTree, profile: &GuestGicProfile) -> AxVmResult {
    let controller = (!profile.node_path.is_empty())
        .then(|| tree.inner().get_by_path_id(&profile.node_path))
        .flatten()
        .or_else(|| find_gicv3(tree))
        .ok_or_else(|| {
            ax_err_type!(
                InvalidData,
                "guest FDT has no GICv3 interrupt-controller node"
            )
        })?;
    tree.inner_mut()
        .view_typed_mut(controller)
        .ok_or_else(|| ax_err_type!(InvalidData, "guest GICv3 node is missing"))?
        .set_regs(&[
            RegInfo::new(
                profile.distributor_base as u64,
                Some(profile.distributor_length as u64),
            ),
            RegInfo::new(
                profile.redistributor_base as u64,
                Some(profile.redistributor_length as u64),
            ),
        ]);
    tree.set_property(controller, prop_u32("#interrupt-cells", 3))?;
    if let Some(phandle) = profile.node_phandle {
        install_controller_phandle(tree, controller, phandle, "GIC")?;
    }
    Ok(())
}

fn install_plic_registers(tree: &mut FdtTree, profile: &GuestPlicProfile) -> AxVmResult {
    let controller = tree
        .inner()
        .get_by_path_id(&profile.node_path)
        .or_else(|| find_plic(tree))
        .ok_or_else(|| {
            ax_err_type!(
                InvalidData,
                "guest FDT has no PLIC interrupt-controller node"
            )
        })?;
    tree.inner_mut()
        .view_typed_mut(controller)
        .ok_or_else(|| ax_err_type!(InvalidData, "guest PLIC node is missing"))?
        .set_regs(&[RegInfo::new(
            profile.base as u64,
            Some(profile.length as u64),
        )]);
    tree.set_property(controller, prop_u32("#interrupt-cells", 1))?;
    if let Some(phandle) = profile.node_phandle {
        install_controller_phandle(tree, controller, phandle, "PLIC")?;
    }
    Ok(())
}

fn install_controller_phandle(
    tree: &mut FdtTree,
    controller: NodeId,
    phandle: u32,
    controller_name: &str,
) -> AxVmResult {
    if let Some(existing) = tree.inner().get_by_phandle(phandle.into())
        && existing.id() != controller
    {
        return Err(ax_err_type!(
            InvalidData,
            alloc::format!(
                "host {controller_name} phandle {phandle:#x} conflicts with another guest node"
            )
        ));
    }
    let old_phandle = tree
        .inner()
        .node(controller)
        .and_then(|node| {
            node.get_property("phandle")
                .or_else(|| node.get_property("linux,phandle"))
        })
        .and_then(Property::get_u32);
    if let Some(old_phandle) = old_phandle.filter(|old| *old != phandle) {
        let references = tree
            .inner()
            .iter_node_ids()
            .filter(|node_id| {
                tree.inner().node(*node_id).is_some_and(|node| {
                    ["interrupt-parent", "msi-parent"].into_iter().any(|name| {
                        node.get_property(name).and_then(Property::get_u32) == Some(old_phandle)
                    })
                })
            })
            .collect::<alloc::vec::Vec<_>>();
        for node_id in references {
            for name in ["interrupt-parent", "msi-parent"] {
                let matches = tree
                    .inner()
                    .node(node_id)
                    .and_then(|node| node.get_property(name))
                    .and_then(Property::get_u32)
                    == Some(old_phandle);
                if matches {
                    tree.set_property(node_id, prop_u32(name, phandle))?;
                }
            }
        }
    }
    tree.set_property(controller, prop_u32("phandle", phandle))?;
    tree.set_property(controller, prop_u32("linux,phandle", phandle))
}

fn prop_u32(name: &str, value: u32) -> Property {
    let mut property = Property::new(name, alloc::vec![]);
    property.set_u32_ls(&[value]);
    property
}

fn find_gicv3(tree: &FdtTree) -> Option<NodeId> {
    tree.inner().iter_node_ids().find(|node_id| {
        tree.inner().node(*node_id).is_some_and(|node| {
            node.get_property("interrupt-controller").is_some()
                && node
                    .compatibles()
                    .any(|compatible| compatible == "arm,gic-v3")
        })
    })
}

fn find_plic(tree: &FdtTree) -> Option<NodeId> {
    find_plic_in_fdt(tree.inner())
}

fn find_plic_in_fdt(fdt: &Fdt) -> Option<NodeId> {
    fdt.iter_node_ids().find(|node_id| {
        fdt.node(*node_id).is_some_and(|node| {
            node.get_property("interrupt-controller").is_some()
                && node
                    .compatibles()
                    .any(|compatible| matches!(compatible, "riscv,plic0" | "sifive,plic-1.0.0"))
        })
    })
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use fdt_edit::{Fdt, Node, Property};

    use super::*;

    #[test]
    fn replaces_host_gic_windows_with_virtual_machine_windows() {
        let mut tree = FdtTree::new();
        let root = tree.inner().root_id();
        let controller = tree.add_node(root, Node::new("intc@8000000"));
        let mut compatible = Property::new("compatible", vec![]);
        compatible.set_string("arm,gic-v3");
        tree.set_property(controller, compatible).unwrap();
        tree.set_property(controller, Property::new("interrupt-controller", vec![]))
            .unwrap();

        let profile = GuestGicProfile {
            node_path: "/intc@8000000".into(),
            node_phandle: Some(1),
            distributor_base: 0x0800_0000,
            distributor_length: 0x1_0000,
            redistributor_base: 0x080a_0000,
            redistributor_length: 0x2_0000,
        };
        install_gic_registers(&mut tree, &profile).unwrap();
        let bytes = tree.finish();
        let fdt = Fdt::from_bytes(&bytes).unwrap();
        let regs = fdt.get_by_path("/intc@8000000").unwrap().regs();

        assert_eq!(regs.len(), 2);
        assert_eq!(
            (regs[0].address, regs[0].size),
            (0x0800_0000, Some(0x1_0000))
        );
        assert_eq!(
            (regs[1].address, regs[1].size),
            (0x080a_0000, Some(0x2_0000))
        );
        let controller = fdt.get_by_path("/intc@8000000").unwrap();
        assert_eq!(
            controller
                .as_node()
                .get_property("linux,phandle")
                .unwrap()
                .get_u32(),
            Some(1)
        );
        assert_eq!(
            controller
                .as_node()
                .get_property("#interrupt-cells")
                .unwrap()
                .get_u32(),
            Some(3)
        );
    }

    #[test]
    fn resolves_host_gic_windows_and_phandle() {
        let mut tree = FdtTree::new();
        let root = tree.inner().root_id();
        tree.set_property(root, prop_u32("#address-cells", 2))
            .unwrap();
        tree.set_property(root, prop_u32("#size-cells", 2)).unwrap();
        let controller = tree.add_node(root, Node::new("interrupt-controller@fe600000"));
        let mut compatible = Property::new("compatible", vec![]);
        compatible.set_string("arm,gic-v3");
        tree.set_property(controller, compatible).unwrap();
        tree.set_property(controller, Property::new("interrupt-controller", vec![]))
            .unwrap();
        tree.set_property(controller, prop_u32("phandle", 1))
            .unwrap();
        tree.inner_mut()
            .view_typed_mut(controller)
            .unwrap()
            .set_regs(&[
                RegInfo::new(0xfe60_0000, Some(0x1_0000)),
                RegInfo::new(0xfe68_0000, Some(0x10_0000)),
            ]);
        let fdt = Fdt::from_bytes(&tree.finish()).unwrap();

        let profile = host_gic_profile(&fdt).unwrap().unwrap();

        assert_eq!(
            profile,
            GuestGicProfile {
                node_path: "/interrupt-controller@fe600000".into(),
                node_phandle: Some(1),
                distributor_base: 0xfe60_0000,
                distributor_length: 0x1_0000,
                redistributor_base: 0xfe68_0000,
                redistributor_length: 0x10_0000,
            }
        );
    }

    #[test]
    fn resolves_and_reuses_host_plic_window_and_phandle() {
        let mut tree = FdtTree::new();
        let root = tree.inner().root_id();
        tree.set_property(root, prop_u32("#address-cells", 2))
            .unwrap();
        tree.set_property(root, prop_u32("#size-cells", 2)).unwrap();
        let soc = tree.add_node(root, Node::new("soc"));
        tree.set_property(soc, prop_u32("#address-cells", 2))
            .unwrap();
        tree.set_property(soc, prop_u32("#size-cells", 2)).unwrap();
        let controller = tree.add_node(soc, Node::new("plic@d000000"));
        let mut compatible = Property::new("compatible", vec![]);
        compatible.set_string("riscv,plic0");
        tree.set_property(controller, compatible).unwrap();
        tree.set_property(controller, Property::new("interrupt-controller", vec![]))
            .unwrap();
        tree.set_property(controller, prop_u32("phandle", 9))
            .unwrap();
        tree.inner_mut()
            .view_typed_mut(controller)
            .unwrap()
            .set_regs(&[RegInfo::new(0x0d00_0000, Some(0x80_0000))]);
        let bytes = tree.finish();
        let fdt = Fdt::from_bytes(&bytes).unwrap();

        let profile = host_plic_profile(&fdt).unwrap().unwrap();
        assert_eq!(
            profile,
            GuestPlicProfile {
                node_path: "/soc/plic@d000000".into(),
                node_phandle: Some(9),
                base: 0x0d00_0000,
                length: 0x80_0000,
            }
        );

        let mut guest = FdtTree::from_bytes(&bytes).unwrap();
        install_plic_registers(&mut guest, &profile).unwrap();
        let guest = Fdt::from_bytes(&guest.finish()).unwrap();
        let controller = guest.get_by_path("/soc/plic@d000000").unwrap();
        assert_eq!(
            controller
                .as_node()
                .get_property("phandle")
                .unwrap()
                .get_u32(),
            Some(9)
        );
        let regs = controller.regs();
        assert_eq!(
            (regs[0].address, regs[0].size),
            (0x0d00_0000, Some(0x80_0000))
        );
        assert_eq!(
            controller
                .as_node()
                .get_property("#interrupt-cells")
                .unwrap()
                .get_u32(),
            Some(1)
        );
    }
}
