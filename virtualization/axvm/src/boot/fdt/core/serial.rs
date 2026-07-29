//! Machine-owned virtual serial description for guest device trees.

use alloc::{format, string::String, vec, vec::Vec};

use axdevice_base::AccessWidth;
use fdt_edit::{Fdt, Node, Property};
use fdt_raw::RegInfo;

use super::tree::{FdtTree, prop_string};
use crate::{
    AxVmResult, ax_err_type,
    machine::{
        GuestSerialFdtIdentity, GuestSerialFdtInterrupt, GuestSerialModel, GuestSerialProfile,
        GuestSerialTransport,
    },
};

pub(crate) struct HostSelectedSerial {
    pub profile: GuestSerialProfile,
    pub identity: GuestSerialFdtIdentity,
}

/// Replaces firmware-provided UARTs with the current machine's virtual UART.
pub(crate) fn install_machine_serial(
    tree: &mut FdtTree,
    profile: GuestSerialProfile,
    identity: Option<&GuestSerialFdtIdentity>,
) -> AxVmResult {
    let machine = crate::machine::current_machine_profile(1);
    let GuestSerialTransport::Mmio { .. } = profile.transport else {
        return Ok(());
    };
    let Some(interrupt_encoding) = machine.serial_fdt_interrupt else {
        return Ok(());
    };
    install_mmio_serial(tree, profile, interrupt_encoding, identity)
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

/// Resolves the guest virtual UART identity from the firmware-selected host UART.
///
/// Firmware-backed machines retain the selected host UART's register model and
/// bus layout while replacing the physical device with an emulated UART.
pub(crate) fn host_selected_serial(
    fdt: &Fdt,
    fallback: GuestSerialProfile,
    interrupt_encoding: GuestSerialFdtInterrupt,
) -> AxVmResult<Option<HostSelectedSerial>> {
    let Some((stdout_selector, path)) = stdout_selection(fdt) else {
        return Ok(None);
    };
    let serial = fdt.get_by_path(&path).ok_or_else(|| {
        ax_err_type!(
            InvalidData,
            format!("host stdout UART node {path} is missing")
        )
    })?;
    let node = serial.as_node();
    let compatibles = node.compatibles().collect::<Vec<_>>();
    let model = if compatibles.contains(&"arm,pl011") {
        GuestSerialModel::Pl011
    } else if compatibles
        .iter()
        .any(|compatible| matches!(*compatible, "ns16550" | "ns16550a" | "snps,dw-apb-uart"))
    {
        GuestSerialModel::Uart16550
    } else {
        return Err(ax_err_type!(
            Unsupported,
            format!(
                "host stdout UART node {path} has no supported virtual register model: \
                 {compatibles:?}"
            )
        ));
    };

    let reg = serial.regs().into_iter().next().ok_or_else(|| {
        ax_err_type!(
            InvalidData,
            format!("host stdout UART node {path} has no register range")
        )
    })?;
    let base = usize::try_from(reg.address).map_err(|_| {
        ax_err_type!(
            InvalidData,
            format!(
                "host stdout UART address does not fit usize: {:#x}",
                reg.address
            )
        )
    })?;
    let length = reg
        .size
        .ok_or_else(|| {
            ax_err_type!(
                InvalidData,
                format!("host stdout UART node {path} has no register range size")
            )
        })
        .and_then(|length| {
            usize::try_from(length).map_err(|_| {
                ax_err_type!(
                    InvalidData,
                    format!("host stdout UART range size does not fit usize: {length:#x}")
                )
            })
        })?;
    if length == 0 {
        return Err(ax_err_type!(
            InvalidData,
            format!("host stdout UART node {path} has an empty register range")
        ));
    }

    let GuestSerialTransport::Mmio { .. } = fallback.transport else {
        return Err(ax_err_type!(
            InvalidData,
            "FDT-backed machine serial profile is not MMIO"
        ));
    };
    let (register_shift, register_width, clock_hz) = match model {
        GuestSerialModel::Pl011 => (0, AccessWidth::Dword, fallback.clock_hz),
        GuestSerialModel::Uart16550 => {
            let shift = node
                .get_property("reg-shift")
                .and_then(Property::get_u32)
                .unwrap_or(0);
            if shift >= usize::BITS {
                return Err(ax_err_type!(
                    InvalidData,
                    format!("host stdout UART reg-shift {shift} is too large")
                ));
            }
            let register_width = node
                .get_property("reg-io-width")
                .and_then(Property::get_u32)
                .map_or(Ok(AccessWidth::Byte), |width| {
                    AccessWidth::try_from(width as usize).map_err(|_| {
                        ax_err_type!(
                            InvalidData,
                            format!("host stdout UART reg-io-width {width} is unsupported")
                        )
                    })
                })?;
            let clock_hz = node
                .get_property("clock-frequency")
                .and_then(Property::get_u32)
                .filter(|clock| *clock != 0)
                .unwrap_or(fallback.clock_hz);
            (shift as u8, register_width, clock_hz)
        }
    };
    let interrupt = serial.interrupts().into_iter().next().ok_or_else(|| {
        ax_err_type!(
            InvalidData,
            format!("host stdout UART node {path} has no interrupt")
        )
    })?;
    let irq = decode_interrupt_id(&path, interrupt_encoding, &interrupt.specifier)?;
    let node_phandle = node
        .get_property("phandle")
        .or_else(|| node.get_property("linux,phandle"))
        .and_then(Property::get_u32);

    Ok(Some(HostSelectedSerial {
        profile: GuestSerialProfile {
            model,
            transport: GuestSerialTransport::Mmio {
                base,
                length,
                register_shift,
                register_width,
            },
            irq,
            clock_hz,
        },
        identity: GuestSerialFdtIdentity {
            node_path: path,
            node_phandle,
            interrupt_parent: interrupt.interrupt_parent.raw(),
            interrupt_specifier: interrupt.specifier,
            stdout_path: stdout_selector,
        },
    }))
}

fn decode_interrupt_id(
    path: &str,
    encoding: GuestSerialFdtInterrupt,
    specifier: &[u32],
) -> AxVmResult<usize> {
    let raw = match encoding {
        GuestSerialFdtInterrupt::GicSpi => {
            if specifier.first().copied() != Some(0) {
                return Err(ax_err_type!(
                    Unsupported,
                    format!("host stdout UART node {path} is not connected to a GIC SPI")
                ));
            }
            specifier
                .get(1)
                .copied()
                .and_then(|source| source.checked_add(32))
                .ok_or_else(|| {
                    ax_err_type!(
                        InvalidData,
                        format!("host stdout UART node {path} has an invalid GIC interrupt")
                    )
                })?
        }
        GuestSerialFdtInterrupt::PlicSource => specifier
            .first()
            .copied()
            .filter(|source| *source != 0)
            .ok_or_else(|| {
                ax_err_type!(
                    InvalidData,
                    format!("host stdout UART node {path} has an invalid PLIC interrupt")
                )
            })?,
    };
    usize::try_from(raw).map_err(|_| {
        ax_err_type!(
            InvalidData,
            format!("host stdout UART interrupt does not fit usize: {raw}")
        )
    })
}

fn install_mmio_serial(
    tree: &mut FdtTree,
    profile: GuestSerialProfile,
    interrupt_encoding: GuestSerialFdtInterrupt,
    identity: Option<&GuestSerialFdtIdentity>,
) -> AxVmResult {
    let GuestSerialTransport::Mmio {
        base,
        length,
        register_shift,
        register_width,
    } = profile.transport
    else {
        return Err(ax_err_type!(
            InvalidData,
            "device-tree serial profile is not MMIO"
        ));
    };
    let interrupt_parent = match identity {
        Some(identity) => identity.interrupt_parent,
        None => interrupt_controller_phandle(tree, interrupt_encoding)?,
    };

    let mut old_paths = physical_serial_paths(tree.inner());
    old_paths.sort_by_key(|path| core::cmp::Reverse(path.matches('/').count()));
    for path in old_paths {
        tree.inner_mut().remove_by_path(&path);
    }

    let serial_path = match identity {
        Some(identity) => identity.node_path.clone(),
        None => match profile.model {
            GuestSerialModel::Pl011 => format!("/pl011@{base:x}"),
            GuestSerialModel::Uart16550 => format!("/serial@{base:x}"),
        },
    };
    let (parent_path, node_name) = serial_path.rsplit_once('/').ok_or_else(|| {
        ax_err_type!(
            InvalidData,
            format!("virtual serial node path is not absolute: {serial_path}")
        )
    })?;
    let parent = if parent_path.is_empty() {
        tree.inner().root_id()
    } else {
        tree.ensure_path(parent_path)?
    };
    let serial_id = tree.add_node(parent, Node::new(node_name));
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
            tree.set_property(
                serial_id,
                prop_u32("reg-io-width", register_width.size() as u32),
            )?;
        }
    }
    tree.set_property(serial_id, prop_u32("clock-frequency", profile.clock_hz))?;
    tree.set_property(serial_id, prop_u32("current-speed", 115_200))?;
    tree.set_property(serial_id, prop_u32("interrupt-parent", interrupt_parent))?;
    let interrupts = match identity {
        Some(identity) => prop_u32_list("interrupts", &identity.interrupt_specifier),
        None => match interrupt_encoding {
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
    };
    tree.set_property(serial_id, interrupts)?;
    if let Some(phandle) = identity.and_then(|identity| identity.node_phandle) {
        tree.set_property(serial_id, prop_u32("phandle", phandle))?;
        tree.set_property(serial_id, prop_u32("linux,phandle", phandle))?;
    }

    if identity.is_none() {
        let aliases = tree.ensure_path("/aliases")?;
        tree.set_property(aliases, prop_string("serial0", &serial_path))?;
    }
    let chosen = tree.ensure_path("/chosen")?;
    let stdout_path = identity
        .map(|identity| identity.stdout_path.as_str())
        .unwrap_or(&serial_path);
    let stdout_selector = stdout_path.split(':').next().unwrap_or(stdout_path);
    if !stdout_selector.starts_with('/') {
        let aliases = tree.ensure_path("/aliases")?;
        tree.set_property(aliases, prop_string(stdout_selector, &serial_path))?;
    }
    tree.set_property(chosen, prop_string("stdout-path", stdout_path))?;
    Ok(())
}

fn install_pl011_clock(tree: &mut FdtTree, clock_hz: u32) -> AxVmResult<u32> {
    const CLOCK_PATH: &str = "/vuart-clock";

    tree.inner_mut().remove_by_path(CLOCK_PATH);
    let phandle = next_phandle(tree.inner());
    let clock = tree.add_node(tree.inner().root_id(), Node::new("vuart-clock"));

    tree.set_property(clock, prop_string("compatible", "fixed-clock"))?;
    tree.set_property(clock, prop_u32("#clock-cells", 0))?;
    tree.set_property(clock, prop_u32("clock-frequency", clock_hz))?;
    tree.set_property(
        clock,
        prop_string("clock-output-names", "virtual-uart-clock"),
    )?;
    tree.set_property(clock, prop_u32("phandle", phandle))?;
    tree.set_property(clock, prop_u32("linux,phandle", phandle))?;
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
    tree.set_property(controller, prop_u32("linux,phandle", phandle))?;
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

fn stdout_selection(fdt: &Fdt) -> Option<(String, String)> {
    let chosen = fdt.get_by_path("/chosen")?;
    let raw = ["stdout-path", "linux,stdout-path"]
        .into_iter()
        .find_map(|name| chosen.as_node().get_property(name)?.as_str())?;
    let selector = raw.split(':').next().unwrap_or(raw);
    let path = if selector.starts_with('/') {
        selector
    } else {
        fdt.get_by_path("/aliases")?
            .as_node()
            .get_property(selector)?
            .as_str()?
    };
    Some((raw.into(), path.into()))
}

fn stdout_path(fdt: &Fdt) -> Option<String> {
    stdout_selection(fdt).map(|(_, path)| path)
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
        tree.set_property(root, prop_u32("interrupt-parent", 7))
            .unwrap();
        let controller = tree.add_node(root, Node::new(name));
        tree.set_property(controller, prop_string("compatible", compatible))
            .unwrap();
        tree.set_property(controller, Property::new("interrupt-controller", vec![]))
            .unwrap();
        tree.set_property(
            controller,
            prop_u32(
                "#interrupt-cells",
                if compatible.contains("gic") { 3 } else { 1 },
            ),
        )
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
                register_width: AccessWidth::Dword,
            },
            irq: 33,
            clock_hz: 24_000_000,
        };

        install_mmio_serial(&mut tree, profile, GuestSerialFdtInterrupt::GicSpi, None).unwrap();
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
        let clock = fdt.get_by_path("/vuart-clock").unwrap();
        assert!(
            clock
                .as_node()
                .compatibles()
                .any(|value| value == "fixed-clock")
        );
        assert_eq!(
            clock
                .as_node()
                .get_property("#clock-cells")
                .unwrap()
                .get_u32(),
            Some(0)
        );
        assert_eq!(
            clock
                .as_node()
                .get_property("clock-frequency")
                .unwrap()
                .get_u32(),
            Some(24_000_000)
        );
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
                register_width: AccessWidth::Byte,
            },
            irq: 10,
            clock_hz: 3_686_400,
        };

        install_mmio_serial(
            &mut tree,
            profile,
            GuestSerialFdtInterrupt::PlicSource,
            None,
        )
        .unwrap();
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
                .get_property("reg-io-width")
                .unwrap()
                .get_u32(),
            Some(1)
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
                register_width: AccessWidth::Byte,
            },
            irq: 10,
            clock_hz: 3_686_400,
        };
        install_mmio_serial(
            &mut tree,
            profile,
            GuestSerialFdtInterrupt::PlicSource,
            None,
        )
        .unwrap();

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

    #[test]
    fn installs_pl011_with_host_irq_phandle_and_stdout_identity() {
        let mut tree = tree_with_controller("arm,gic-v3", "interrupt-controller@fe600000");
        let root = tree.inner().root_id();
        let host_serial = tree.add_node(root, Node::new("serial@feb50000"));
        tree.set_property(
            host_serial,
            prop_string_list("compatible", &["arm,pl011", "arm,primecell"]),
        )
        .unwrap();
        tree.inner_mut()
            .view_typed_mut(host_serial)
            .unwrap()
            .set_regs(&[RegInfo::new(0xfeb5_0000, Some(0x1000))]);
        tree.set_property(host_serial, prop_u32_list("interrupts", &[0, 0x14d, 4]))
            .unwrap();
        tree.set_property(host_serial, prop_u32("phandle", 0x2d1))
            .unwrap();
        let aliases = tree.ensure_path("/aliases").unwrap();
        tree.set_property(aliases, prop_string("serial2", "/serial@feb50000"))
            .unwrap();
        let chosen = tree.ensure_path("/chosen").unwrap();
        tree.set_property(chosen, prop_string("stdout-path", "serial2:1500000"))
            .unwrap();

        let host_dtb = tree.finish();
        let host_fdt = Fdt::from_bytes(&host_dtb).unwrap();
        let fallback = GuestSerialProfile {
            model: GuestSerialModel::Pl011,
            transport: GuestSerialTransport::Mmio {
                base: 0x0900_0000,
                length: 0x1000,
                register_shift: 0,
                register_width: AccessWidth::Dword,
            },
            irq: 33,
            clock_hz: 24_000_000,
        };
        let resolved = host_selected_serial(&host_fdt, fallback, GuestSerialFdtInterrupt::GicSpi)
            .unwrap()
            .unwrap();
        assert_eq!(resolved.profile.model, GuestSerialModel::Pl011);

        let mut tree = FdtTree::from_bytes(&host_dtb).unwrap();
        install_mmio_serial(
            &mut tree,
            resolved.profile,
            GuestSerialFdtInterrupt::GicSpi,
            Some(&resolved.identity),
        )
        .unwrap();
        let fdt = Fdt::from_bytes(&tree.finish()).unwrap();
        let serial = fdt.get_by_path("/serial@feb50000").unwrap();

        assert!(
            serial
                .as_node()
                .compatibles()
                .any(|value| value == "arm,pl011")
        );
        assert!(serial.as_node().get_property("reg-shift").is_none());
        assert!(serial.as_node().get_property("reg-io-width").is_none());
        assert_eq!(serial.regs()[0].address, 0xfeb5_0000);
        assert_eq!(serial.regs()[0].size, Some(0x1000));
        assert_eq!(
            serial.as_node().get_property("phandle").unwrap().get_u32(),
            Some(0x2d1)
        );
        assert_eq!(
            serial
                .as_node()
                .get_property("linux,phandle")
                .unwrap()
                .get_u32(),
            Some(0x2d1)
        );
        assert_eq!(
            serial
                .as_node()
                .get_property("interrupt-parent")
                .unwrap()
                .get_u32(),
            Some(7)
        );
        assert_eq!(
            serial
                .as_node()
                .get_property("interrupts")
                .unwrap()
                .get_u32_iter()
                .collect::<Vec<_>>(),
            [0, 0x14d, 4]
        );
        assert_eq!(
            fdt.get_by_path("/aliases")
                .unwrap()
                .as_node()
                .get_property("serial2")
                .unwrap()
                .as_str(),
            Some("/serial@feb50000")
        );
        assert_eq!(
            fdt.get_by_path("/chosen")
                .unwrap()
                .as_node()
                .get_property("stdout-path")
                .unwrap()
                .as_str(),
            Some("serial2:1500000")
        );
    }

    #[test]
    fn resolves_dw_apb_uart_as_virtual_16550() {
        let mut tree = FdtTree::new();
        let root = tree.inner().root_id();
        tree.set_property(root, prop_u32("#address-cells", 2))
            .unwrap();
        tree.set_property(root, prop_u32("#size-cells", 2)).unwrap();
        tree.set_property(root, prop_u32("interrupt-parent", 1))
            .unwrap();
        let gic = tree.add_node(root, Node::new("interrupt-controller@fe600000"));
        tree.set_property(gic, prop_string("compatible", "arm,gic-v3"))
            .unwrap();
        tree.set_property(gic, Property::new("interrupt-controller", vec![]))
            .unwrap();
        tree.set_property(gic, prop_u32("#interrupt-cells", 3))
            .unwrap();
        tree.set_property(gic, prop_u32("phandle", 1)).unwrap();
        let serial = tree.add_node(root, Node::new("serial@feb50000"));
        tree.set_property(
            serial,
            prop_string_list("compatible", &["rockchip,rk3588-uart", "snps,dw-apb-uart"]),
        )
        .unwrap();
        tree.inner_mut()
            .view_typed_mut(serial)
            .unwrap()
            .set_regs(&[RegInfo::new(0xfeb5_0000, Some(0x100))]);
        tree.set_property(serial, prop_u32("reg-shift", 2)).unwrap();
        tree.set_property(serial, prop_u32("reg-io-width", 4))
            .unwrap();
        tree.set_property(serial, prop_u32_list("interrupts", &[0, 0x14d, 4]))
            .unwrap();
        tree.set_property(serial, prop_u32("phandle", 0x2d1))
            .unwrap();
        let chosen = tree.ensure_path("/chosen").unwrap();
        tree.set_property(
            chosen,
            prop_string("stdout-path", "/serial@feb50000:1500000"),
        )
        .unwrap();

        let host_dtb = tree.finish();
        let host_fdt = Fdt::from_bytes(&host_dtb).unwrap();
        let fallback = GuestSerialProfile {
            model: GuestSerialModel::Pl011,
            transport: GuestSerialTransport::Mmio {
                base: 0x0900_0000,
                length: 0x1000,
                register_shift: 0,
                register_width: AccessWidth::Dword,
            },
            irq: 33,
            clock_hz: 24_000_000,
        };

        let resolved = host_selected_serial(&host_fdt, fallback, GuestSerialFdtInterrupt::GicSpi)
            .unwrap()
            .unwrap();

        assert_eq!(
            resolved.profile,
            GuestSerialProfile {
                model: GuestSerialModel::Uart16550,
                transport: GuestSerialTransport::Mmio {
                    base: 0xfeb5_0000,
                    length: 0x100,
                    register_shift: 2,
                    register_width: AccessWidth::Dword,
                },
                irq: 365,
                clock_hz: 24_000_000,
            }
        );
        assert_eq!(resolved.identity.node_path, "/serial@feb50000");
        assert_eq!(resolved.identity.node_phandle, Some(0x2d1));
        assert_eq!(resolved.identity.interrupt_parent, 1);
        assert_eq!(resolved.identity.interrupt_specifier, [0, 0x14d, 4]);
        assert_eq!(resolved.identity.stdout_path, "/serial@feb50000:1500000");

        let mut tree = FdtTree::from_bytes(&host_dtb).unwrap();
        install_mmio_serial(
            &mut tree,
            resolved.profile,
            GuestSerialFdtInterrupt::GicSpi,
            Some(&resolved.identity),
        )
        .unwrap();
        let guest_fdt = Fdt::from_bytes(&tree.finish()).unwrap();
        let guest_serial = guest_fdt.get_by_path("/serial@feb50000").unwrap();
        assert!(
            guest_serial
                .as_node()
                .compatibles()
                .any(|compatible| compatible == "ns16550a")
        );
        assert_eq!(
            guest_serial
                .as_node()
                .get_property("reg-shift")
                .unwrap()
                .get_u32(),
            Some(2)
        );
        assert_eq!(
            guest_serial
                .as_node()
                .get_property("reg-io-width")
                .unwrap()
                .get_u32(),
            Some(4)
        );
        assert!(guest_fdt.get_by_path("/vuart-clock").is_none());
    }
}
