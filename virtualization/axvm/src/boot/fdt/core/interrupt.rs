//! Machine-owned interrupt-controller description for guest device trees.

use axvm_types::EmulatedDeviceType;
use fdt_edit::NodeId;
use fdt_raw::RegInfo;

use super::tree::FdtTree;
use crate::{AxVmResult, ax_err_type};

/// Rewrites the GICv3 register windows to match the VM-owned controller.
pub(crate) fn install_machine_interrupt_controller(
    tree: &mut FdtTree,
    cpu_num: usize,
) -> AxVmResult {
    let machine = crate::machine::current_machine_profile(cpu_num);
    let distributor = machine
        .emulated_devices
        .iter()
        .find(|device| device.emu_type == EmulatedDeviceType::InterruptController);
    let redistributor = machine
        .emulated_devices
        .iter()
        .find(|device| device.emu_type == EmulatedDeviceType::ArmGicRedistributor);

    let (Some(distributor), Some(redistributor)) = (distributor, redistributor) else {
        return Ok(());
    };
    install_gic_registers(
        tree,
        distributor.base_gpa,
        distributor.length,
        redistributor.base_gpa,
        redistributor.length,
    )
}

fn install_gic_registers(
    tree: &mut FdtTree,
    distributor_base: usize,
    distributor_length: usize,
    redistributor_base: usize,
    redistributor_length: usize,
) -> AxVmResult {
    let controller = find_gicv3(tree).ok_or_else(|| {
        ax_err_type!(
            InvalidData,
            "guest FDT has no GICv3 interrupt-controller node"
        )
    })?;
    tree.inner_mut()
        .view_typed_mut(controller)
        .ok_or_else(|| ax_err_type!(InvalidData, "guest GICv3 node is missing"))?
        .set_regs(&[
            RegInfo::new(distributor_base as u64, Some(distributor_length as u64)),
            RegInfo::new(redistributor_base as u64, Some(redistributor_length as u64)),
        ]);
    Ok(())
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

        install_gic_registers(&mut tree, 0x0800_0000, 0x1_0000, 0x080a_0000, 0x2_0000).unwrap();
        let fdt = Fdt::from_bytes(&tree.finish()).unwrap();
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
    }
}
