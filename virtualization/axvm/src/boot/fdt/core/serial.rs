//! Machine-owned virtual serial description for guest device trees.

use alloc::{format, string::String, vec, vec::Vec};

use fdt_edit::{Fdt, Node, Property};
use fdt_raw::RegInfo;

use super::tree::{FdtTree, prop_string};
use crate::{
    AxVmResult, ax_err_type,
    machine::{
        GuestSerialFdtInterrupt, GuestSerialModel, GuestSerialProfile, GuestSerialTransport,
    },
};

/// Replaces firmware-provided UARTs with the current machine's virtual UART.
pub(crate) fn install_machine_serial(tree: &mut FdtTree) -> AxVmResult {
    let machine = crate::machine::current_machine_profile(1);
    let profile = machine.serial;
    let GuestSerialTransport::Mmio { .. } = profile.transport else {
        return Ok(());
    };
    let Some(interrupt_encoding) = machine.serial_fdt_interrupt else {
        return Ok(());
    };
    install_mmio_serial(tree, profile, interrupt_encoding)
}

/// Returns physical UART nodes that must remain owned by the host.
pub(crate) fn physical_serial_paths(fdt: &Fdt) -> Vec<String> {
    let stdout_path = stdout_path(fdt);
    let mut paths = fdt
        .iter_node_ids()
        .filter_map(|node_id| {
            let node = fdt.node(node_id)?;
            let path = fdt.path_of(node_id);
            let serial_name = node.name().starts_with("serial@")
                || node.name().starts_with("uart@")
                || node.name().starts_with("pl011@");
            let serial_compatible = node.compatibles().any(|compatible| {
                compatible.contains("uart")
                    || compatible.contains("serial")
                    || compatible == "arm,pl011"
                    || compatible == "ns16550"
                    || compatible == "ns16550a"
            });
            (serial_name || serial_compatible || stdout_path.as_deref() == Some(path.as_str()))
                .then_some(path)
        })
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();
    paths
}

fn install_mmio_serial(
    tree: &mut FdtTree,
    profile: GuestSerialProfile,
    interrupt_encoding: GuestSerialFdtInterrupt,
) -> AxVmResult {
    let GuestSerialTransport::Mmio {
        base,
        length,
        register_shift,
    } = profile.transport
    else {
        return Err(ax_err_type!(
            InvalidData,
            "device-tree serial profile is not MMIO"
        ));
    };
    let interrupt_parent = interrupt_controller_phandle(tree, interrupt_encoding)?;

    let mut old_paths = physical_serial_paths(tree.inner());
    old_paths.sort_by_key(|path| core::cmp::Reverse(path.matches('/').count()));
    for path in old_paths {
        tree.inner_mut().remove_by_path(&path);
    }

    let serial_path = match profile.model {
        GuestSerialModel::Pl011 => format!("/pl011@{base:x}"),
        GuestSerialModel::Uart16550 => format!("/serial@{base:x}"),
    };
    let serial_id = tree.add_node(tree.inner().root_id(), Node::new(&serial_path[1..]));
    tree.inner_mut()
        .view_typed_mut(serial_id)
        .ok_or_else(|| ax_err_type!(InvalidData, "new serial FDT node is missing"))?
        .set_regs(&[RegInfo::new(base as u64, Some(length as u64))]);

    match profile.model {
        GuestSerialModel::Pl011 => {
            let clock = install_pl011_clock(tree, profile.clock_hz)?;
            tree.set_property(
                serial_id,
                prop_string_list("compatible", &["arm,pl011", "arm,primecell"]),
            )?;
            tree.set_property(serial_id, prop_u32_list("clocks", &[clock, clock]))?;
            tree.set_property(
                serial_id,
                prop_string_list("clock-names", &["uartclk", "apb_pclk"]),
            )?;
        }
        GuestSerialModel::Uart16550 => {
            tree.set_property(serial_id, prop_string("compatible", "ns16550a"))?;
            tree.set_property(serial_id, prop_u32("reg-shift", u32::from(register_shift)))?;
        }
    }
    tree.set_property(serial_id, prop_u32("clock-frequency", profile.clock_hz))?;
    tree.set_property(serial_id, prop_u32("current-speed", 115_200))?;
    tree.set_property(serial_id, prop_u32("interrupt-parent", interrupt_parent))?;
    tree.set_property(
        serial_id,
        match interrupt_encoding {
            GuestSerialFdtInterrupt::GicSpi => {
                let spi = profile.irq.checked_sub(32).ok_or_else(|| {
                    ax_err_type!(InvalidData, "PL011 interrupt ID is not a GIC SPI")
                })?;
                prop_u32_list("interrupts", &[0, spi as u32, 4])
            }
            GuestSerialFdtInterrupt::PlicSource => {
                prop_u32_list("interrupts", &[profile.irq as u32])
            }
        },
    )?;

    let aliases = tree.ensure_path("/aliases")?;
    tree.set_property(aliases, prop_string("serial0", &serial_path))?;
    let chosen = tree.ensure_path("/chosen")?;
    tree.set_property(chosen, prop_string("stdout-path", &serial_path))?;
    Ok(())
}

fn install_pl011_clock(tree: &mut FdtTree, clock_hz: u32) -> AxVmResult<u32> {
    let clock = tree.ensure_path("/apb-pclk")?;
    let phandle = tree
        .inner()
        .node(clock)
        .and_then(|node| {
            node.get_property("phandle")
                .or_else(|| node.get_property("linux,phandle"))
        })
        .and_then(Property::get_u32)
        .unwrap_or_else(|| next_phandle(tree.inner()));

    tree.set_property(clock, prop_string("compatible", "fixed-clock"))?;
    tree.set_property(clock, prop_u32("#clock-cells", 0))?;
    tree.set_property(clock, prop_u32("clock-frequency", clock_hz))?;
    tree.set_property(clock, prop_string("clock-output-names", "clk24mhz"))?;
    tree.set_property(clock, prop_u32("phandle", phandle))?;
    Ok(phandle)
}

fn interrupt_controller_phandle(
    tree: &mut FdtTree,
    encoding: GuestSerialFdtInterrupt,
) -> AxVmResult<u32> {
    let controller = tree
        .inner()
        .iter_node_ids()
        .find(|node_id| {
            let Some(node) = tree.inner().node(*node_id) else {
                return false;
            };
            if node.get_property("interrupt-controller").is_none() {
                return false;
            }
            node.compatibles().any(|compatible| match encoding {
                GuestSerialFdtInterrupt::GicSpi => compatible.contains("gic"),
                GuestSerialFdtInterrupt::PlicSource => compatible.contains("plic"),
            })
        })
        .ok_or_else(|| {
            ax_err_type!(
                InvalidData,
                "guest FDT has no interrupt controller for the machine serial port"
            )
        })?;

    if let Some(phandle) = tree
        .inner()
        .node(controller)
        .and_then(|node| {
            node.get_property("phandle")
                .or_else(|| node.get_property("linux,phandle"))
        })
        .and_then(Property::get_u32)
    {
        return Ok(phandle);
    }

    let phandle = next_phandle(tree.inner());
    tree.set_property(controller, prop_u32("phandle", phandle))?;
    Ok(phandle)
}

fn next_phandle(fdt: &Fdt) -> u32 {
    fdt.iter_node_ids()
        .filter_map(|node_id| {
            let node = fdt.node(node_id)?;
            node.get_property("phandle")
                .or_else(|| node.get_property("linux,phandle"))
                .and_then(Property::get_u32)
        })
        .max()
        .unwrap_or(0)
        .saturating_add(1)
        .max(1)
}

fn stdout_path(fdt: &Fdt) -> Option<String> {
    let chosen = fdt.get_by_path("/chosen")?;
    let raw = ["stdout-path", "linux,stdout-path"]
        .into_iter()
        .find_map(|name| chosen.as_node().get_property(name)?.as_str())?;
    let selector = raw.split(':').next().unwrap_or(raw);
    if selector.starts_with('/') {
        return Some(selector.into());
    }
    fdt.get_by_path("/aliases")?
        .as_node()
        .get_property(selector)?
        .as_str()
        .map(Into::into)
}

fn prop_u32(name: &str, value: u32) -> Property {
    prop_u32_list(name, &[value])
}

fn prop_u32_list(name: &str, values: &[u32]) -> Property {
    let mut prop = Property::new(name, vec![]);
    prop.set_u32_ls(values);
    prop
}

fn prop_string_list(name: &str, values: &[&str]) -> Property {
    let mut prop = Property::new(name, vec![]);
    prop.set_string_ls(values);
    prop
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree_with_controller(compatible: &str, name: &str) -> FdtTree {
        let mut tree = FdtTree::new();
        let root = tree.inner().root_id();
        tree.set_property(root, prop_u32("#address-cells", 2))
            .unwrap();
        tree.set_property(root, prop_u32("#size-cells", 2)).unwrap();
        let controller = tree.add_node(root, Node::new(name));
        tree.set_property(controller, prop_string("compatible", compatible))
            .unwrap();
        tree.set_property(controller, Property::new("interrupt-controller", vec![]))
            .unwrap();
        tree.set_property(controller, prop_u32("phandle", 7))
            .unwrap();
        tree
    }

    #[test]
    fn installs_pl011_with_gic_spi_and_stdout_path() {
        let mut tree = tree_with_controller("arm,gic-v3", "intc@8000000");
        let profile = GuestSerialProfile {
            model: GuestSerialModel::Pl011,
            transport: GuestSerialTransport::Mmio {
                base: 0x0900_0000,
                length: 0x1000,
                register_shift: 0,
            },
            irq: 33,
            clock_hz: 24_000_000,
        };

        install_mmio_serial(&mut tree, profile, GuestSerialFdtInterrupt::GicSpi).unwrap();
        let fdt = Fdt::from_bytes(&tree.finish()).unwrap();
        let serial = fdt.get_by_path("/pl011@9000000").unwrap();
        let regs = serial.regs();

        assert!(
            serial
                .as_node()
                .compatibles()
                .any(|value| value == "arm,pl011")
        );
        assert_eq!(regs.len(), 1);
        assert_eq!(regs[0].address, 0x0900_0000);
        assert_eq!(regs[0].size, Some(0x1000));
        assert_eq!(
            serial
                .as_node()
                .get_property("clock-frequency")
                .unwrap()
                .get_u32(),
            Some(24_000_000)
        );
        assert_eq!(
            serial
                .as_node()
                .get_property("current-speed")
                .unwrap()
                .get_u32(),
            Some(115_200)
        );
        assert_eq!(
            serial
                .as_node()
                .get_property("interrupts")
                .unwrap()
                .get_u32_iter()
                .collect::<Vec<_>>(),
            [0, 1, 4]
        );
        let clock = fdt.get_by_path("/apb-pclk").unwrap();
        let clock_phandle = clock
            .as_node()
            .get_property("phandle")
            .unwrap()
            .get_u32()
            .unwrap();
        assert_eq!(
            serial
                .as_node()
                .get_property("clocks")
                .unwrap()
                .get_u32_iter()
                .collect::<Vec<_>>(),
            [clock_phandle, clock_phandle]
        );
        assert_eq!(
            serial
                .as_node()
                .get_property("clock-names")
                .unwrap()
                .as_str_iter()
                .collect::<Vec<_>>(),
            ["uartclk", "apb_pclk"]
        );
        assert_eq!(
            fdt.get_by_path("/chosen")
                .unwrap()
                .as_node()
                .get_property("stdout-path")
                .unwrap()
                .as_str(),
            Some("/pl011@9000000")
        );
    }

    #[test]
    fn installs_ns16550a_with_plic_source() {
        let mut tree = tree_with_controller("riscv,plic0", "plic@c000000");
        let profile = GuestSerialProfile {
            model: GuestSerialModel::Uart16550,
            transport: GuestSerialTransport::Mmio {
                base: 0x1000_0000,
                length: 0x100,
                register_shift: 0,
            },
            irq: 10,
            clock_hz: 3_686_400,
        };

        install_mmio_serial(&mut tree, profile, GuestSerialFdtInterrupt::PlicSource).unwrap();
        let fdt = Fdt::from_bytes(&tree.finish()).unwrap();
        let serial = fdt.get_by_path("/serial@10000000").unwrap();
        let regs = serial.regs();

        assert!(
            serial
                .as_node()
                .compatibles()
                .any(|value| value == "ns16550a")
        );
        assert_eq!(regs.len(), 1);
        assert_eq!(regs[0].address, 0x1000_0000);
        assert_eq!(regs[0].size, Some(0x100));
        assert_eq!(
            serial
                .as_node()
                .get_property("reg-shift")
                .unwrap()
                .get_u32(),
            Some(0)
        );
        assert_eq!(
            serial
                .as_node()
                .get_property("clock-frequency")
                .unwrap()
                .get_u32(),
            Some(3_686_400)
        );
        assert_eq!(
            serial
                .as_node()
                .get_property("current-speed")
                .unwrap()
                .get_u32(),
            Some(115_200)
        );
        assert_eq!(
            serial
                .as_node()
                .get_property("interrupts")
                .unwrap()
                .get_u32_iter()
                .collect::<Vec<_>>(),
            [10]
        );
    }

    #[test]
    fn replaces_host_serial_nodes_and_console_aliases() {
        let mut tree = tree_with_controller("riscv,plic0", "plic@c000000");
        let soc = tree.ensure_path("/soc").unwrap();
        let old_uart = tree.add_node(soc, Node::new("uart@1000"));
        tree.set_property(old_uart, prop_string("compatible", "ns16550a"))
            .unwrap();
        let old_pl011 = tree.add_node(tree.inner().root_id(), Node::new("debug@2000"));
        tree.set_property(old_pl011, prop_string("compatible", "arm,pl011"))
            .unwrap();
        let aliases = tree.ensure_path("/aliases").unwrap();
        tree.set_property(aliases, prop_string("uart0", "/soc/uart@1000"))
            .unwrap();
        tree.set_property(aliases, prop_string("serial0", "/debug@2000"))
            .unwrap();
        let chosen = tree.ensure_path("/chosen").unwrap();
        tree.set_property(chosen, prop_string("stdout-path", "uart0:115200n8"))
            .unwrap();

        let profile = GuestSerialProfile {
            model: GuestSerialModel::Uart16550,
            transport: GuestSerialTransport::Mmio {
                base: 0x1000_0000,
                length: 0x100,
                register_shift: 0,
            },
            irq: 10,
            clock_hz: 3_686_400,
        };
        install_mmio_serial(&mut tree, profile, GuestSerialFdtInterrupt::PlicSource).unwrap();

        let fdt = Fdt::from_bytes(&tree.finish()).unwrap();
        assert!(fdt.get_by_path("/soc/uart@1000").is_none());
        assert!(fdt.get_by_path("/debug@2000").is_none());
        assert!(fdt.get_by_path("/serial@10000000").is_some());
        assert_eq!(
            fdt.get_by_path("/aliases")
                .unwrap()
                .as_node()
                .get_property("serial0")
                .unwrap()
                .as_str(),
            Some("/serial@10000000")
        );
        assert_eq!(
            fdt.get_by_path("/chosen")
                .unwrap()
                .as_node()
                .get_property("stdout-path")
                .unwrap()
                .as_str(),
            Some("/serial@10000000")
        );
    }
}
