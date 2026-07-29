//! Architecture machine profiles for guest-visible platform devices.
//!
//! User configuration selects physical devices only. Virtual platform resources
//! are owned by the machine profile; firmware-backed hosts may replace the
//! default virtual UART with the host-selected UART's register model, layout,
//! address, and firmware identity before VM construction.

use alloc::{string::String, vec, vec::Vec};

use axdevice_base::AccessWidth;
use axvm_types::{EmulatedDeviceConfig, EmulatedDeviceType};

use crate::{arch::CurrentArch, architecture::MachinePlatform};

pub(crate) const AARCH64_GIC_REDISTRIBUTOR_FRAME_SIZE: usize = 0x2_0000;

/// Guest-visible serial register model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuestSerialModel {
    /// 16550-compatible UART.
    Uart16550,
    /// Arm PrimeCell PL011 UART.
    Pl011,
}

/// Guest-visible serial register transport.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuestSerialTransport {
    /// x86 port I/O range.
    Port { base: u16, length: u16 },
    /// Memory-mapped register range.
    Mmio {
        base: usize,
        length: usize,
        /// Address stride expressed as a power-of-two register shift.
        register_shift: u8,
        /// Bus width used to access one register.
        register_width: AccessWidth,
    },
}

/// Machine-owned serial resources selected for one guest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GuestSerialProfile {
    /// Guest-visible UART model.
    pub model: GuestSerialModel,
    /// Register transport and address range.
    pub transport: GuestSerialTransport,
    /// Virtual interrupt-controller input used by the UART.
    pub irq: usize,
    /// UART reference clock in hertz.
    pub clock_hz: u32,
}

/// Firmware identity retained when a host UART is replaced by a virtual UART.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuestSerialFdtIdentity {
    /// Absolute path of the firmware-selected UART node.
    pub node_path: String,
    /// UART node phandle, when supplied by firmware.
    pub node_phandle: Option<u32>,
    /// Effective interrupt-controller phandle.
    pub interrupt_parent: u32,
    /// Raw firmware interrupt specifier.
    pub interrupt_specifier: Vec<u32>,
    /// Original `stdout-path` selector, including any line settings.
    pub stdout_path: String,
}

/// Host firmware resources retained by the virtual GIC.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuestGicProfile {
    /// Absolute path of the host GICv3 node.
    pub node_path: String,
    /// GIC node phandle, when supplied by firmware.
    pub node_phandle: Option<u32>,
    /// Guest-visible distributor base.
    pub distributor_base: usize,
    /// Guest-visible distributor span.
    pub distributor_length: usize,
    /// Guest-visible redistributor base.
    pub redistributor_base: usize,
    /// Guest-visible redistributor span.
    pub redistributor_length: usize,
}

/// Host firmware resources retained by the virtual PLIC.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuestPlicProfile {
    /// Absolute path of the host PLIC node.
    pub node_path: String,
    /// PLIC node phandle, when supplied by firmware.
    pub node_phandle: Option<u32>,
    /// Guest-visible PLIC register base.
    pub base: usize,
    /// Guest-visible PLIC register span.
    pub length: usize,
}

/// Interrupt encoding used when the common FDT pipeline describes a UART.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuestSerialFdtInterrupt {
    /// Arm GIC SPI tuple.
    GicSpi,
    /// RISC-V PLIC source number.
    PlicSource,
}

/// Virtual platform resources for one architecture machine.
#[derive(Clone, Debug)]
pub struct MachineProfile {
    /// Mandatory guest serial port.
    pub serial: GuestSerialProfile,
    /// Common FDT interrupt encoding, if this machine uses the common FDT path.
    pub serial_fdt_interrupt: Option<GuestSerialFdtInterrupt>,
    /// Physical-device discovery root used for default passthrough assignment.
    ///
    /// `None` means that the architecture's address-space policy alone
    /// provides the default mapping and no unresolved discovery selector
    /// should enter the runtime mapping planner.
    pub default_passthrough_device_path: Option<&'static str>,
    /// Internal device construction descriptors.
    pub emulated_devices: Vec<EmulatedDeviceConfig>,
}

fn device(
    name: &str,
    base_gpa: usize,
    length: usize,
    irq_id: usize,
    emu_type: EmulatedDeviceType,
    cfg_list: Vec<usize>,
) -> EmulatedDeviceConfig {
    EmulatedDeviceConfig {
        name: name.into(),
        base_gpa,
        length,
        irq_id,
        emu_type,
        cfg_list,
    }
}

pub(crate) fn serial_device_config(serial: GuestSerialProfile) -> EmulatedDeviceConfig {
    let (name, base_gpa, length, cfg_list) = match (serial.model, serial.transport) {
        (GuestSerialModel::Uart16550, GuestSerialTransport::Port { base, length }) => {
            ("com1", usize::from(base), usize::from(length), vec![])
        }
        (
            GuestSerialModel::Uart16550,
            GuestSerialTransport::Mmio {
                base,
                length,
                register_shift,
                register_width,
            },
        ) => (
            "uart",
            base,
            length,
            vec![
                serial.clock_hz as usize,
                usize::from(register_shift),
                register_width.size(),
            ],
        ),
        (GuestSerialModel::Pl011, GuestSerialTransport::Mmio { base, length, .. }) => {
            ("pl011", base, length, vec![serial.clock_hz as usize])
        }
        (GuestSerialModel::Pl011, GuestSerialTransport::Port { base, length }) => (
            "pl011",
            usize::from(base),
            usize::from(length),
            vec![serial.clock_hz as usize],
        ),
    };
    device(
        name,
        base_gpa,
        length,
        serial.irq,
        EmulatedDeviceType::Console,
        cfg_list,
    )
}

fn x86_64_profile() -> MachineProfile {
    let serial = GuestSerialProfile {
        model: GuestSerialModel::Uart16550,
        transport: GuestSerialTransport::Port {
            base: 0x3f8,
            length: 8,
        },
        irq: 4,
        clock_hz: 1_843_200,
    };
    MachineProfile {
        serial,
        serial_fdt_interrupt: None,
        default_passthrough_device_path: None,
        emulated_devices: vec![
            serial_device_config(serial),
            device(
                "ioapic",
                0xfec0_0000,
                0x1000,
                0,
                EmulatedDeviceType::X86IoApic,
                vec![],
            ),
            device("pit", 0x40, 0x22, 0, EmulatedDeviceType::X86Pit, vec![]),
        ],
    }
}

fn aarch64_profile(cpu_num: usize) -> MachineProfile {
    let cpu_num = cpu_num.max(1);
    let serial = GuestSerialProfile {
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
    MachineProfile {
        serial,
        serial_fdt_interrupt: Some(GuestSerialFdtInterrupt::GicSpi),
        default_passthrough_device_path: Some("/"),
        emulated_devices: vec![
            device(
                "vgic",
                0x0800_0000,
                0x1_0000,
                0,
                EmulatedDeviceType::InterruptController,
                vec![],
            ),
            device(
                "gic-redistributor",
                0x080a_0000,
                cpu_num.saturating_mul(AARCH64_GIC_REDISTRIBUTOR_FRAME_SIZE),
                0,
                EmulatedDeviceType::ArmGicRedistributor,
                vec![cpu_num],
            ),
            serial_device_config(serial),
        ],
    }
}

fn riscv64_profile(cpu_num: usize) -> MachineProfile {
    let serial = GuestSerialProfile {
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
    MachineProfile {
        serial,
        serial_fdt_interrupt: Some(GuestSerialFdtInterrupt::PlicSource),
        default_passthrough_device_path: Some("/"),
        emulated_devices: vec![
            device(
                "plic",
                0x0c00_0000,
                0x60_0000,
                0,
                EmulatedDeviceType::PPPTGlobal,
                vec![cpu_num * 2],
            ),
            serial_device_config(serial),
        ],
    }
}

fn loongarch64_profile() -> MachineProfile {
    let serial = GuestSerialProfile {
        model: GuestSerialModel::Uart16550,
        transport: GuestSerialTransport::Mmio {
            base: 0x1fe0_01e0,
            length: 0x100,
            register_shift: 0,
            register_width: AccessWidth::Byte,
        },
        irq: 2,
        clock_hz: 100_000_000,
    };
    MachineProfile {
        serial,
        serial_fdt_interrupt: None,
        default_passthrough_device_path: Some("/"),
        emulated_devices: vec![
            device(
                "fw_cfg",
                0x1e02_0000,
                0x18,
                0,
                EmulatedDeviceType::FwCfg,
                vec![],
            ),
            device(
                "pch-pic",
                0x1000_0000,
                0x1000,
                0,
                EmulatedDeviceType::LoongArchPchPic,
                vec![],
            ),
            serial_device_config(serial),
        ],
    }
}

/// Architecture identity used by host tools that inspect machine profiles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MachineArchitecture {
    X86_64,
    Aarch64,
    Riscv64,
    LoongArch64,
}

/// Returns the fixed profile for an architecture machine.
pub fn machine_profile_for(architecture: MachineArchitecture, cpu_num: usize) -> MachineProfile {
    match architecture {
        MachineArchitecture::X86_64 => x86_64_profile(),
        MachineArchitecture::Aarch64 => aarch64_profile(cpu_num),
        MachineArchitecture::Riscv64 => riscv64_profile(cpu_num),
        MachineArchitecture::LoongArch64 => loongarch64_profile(),
    }
}

/// Returns the machine profile selected by the architecture boundary.
pub fn current_machine_profile(cpu_num: usize) -> MachineProfile {
    machine_profile_for(CurrentArch::MACHINE_ARCHITECTURE, cpu_num)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_machine_profiles_have_exactly_one_serial_device() {
        for profile in [
            x86_64_profile(),
            aarch64_profile(1),
            riscv64_profile(2),
            loongarch64_profile(),
        ] {
            let serial_devices = profile
                .emulated_devices
                .iter()
                .filter(|device| device.emu_type == EmulatedDeviceType::Console)
                .collect::<Vec<_>>();
            assert_eq!(serial_devices.len(), 1);

            let serial_device = serial_devices[0];
            let (base, length) = match profile.serial.transport {
                GuestSerialTransport::Port { base, length } => {
                    (usize::from(base), usize::from(length))
                }
                GuestSerialTransport::Mmio { base, length, .. } => (base, length),
            };
            assert_eq!(serial_device.base_gpa, base);
            assert_eq!(serial_device.length, length);
            assert_eq!(serial_device.irq_id, profile.serial.irq);
        }
    }

    #[test]
    fn only_device_discovery_machines_emit_a_default_root_selector() {
        assert_eq!(x86_64_profile().default_passthrough_device_path, None);
        assert_eq!(
            aarch64_profile(1).default_passthrough_device_path,
            Some("/")
        );
        assert_eq!(
            riscv64_profile(2).default_passthrough_device_path,
            Some("/")
        );
        assert_eq!(
            loongarch64_profile().default_passthrough_device_path,
            Some("/")
        );
    }

    #[test]
    fn machine_serial_resources_match_guest_platform_contract() {
        assert_eq!(
            x86_64_profile().serial,
            GuestSerialProfile {
                model: GuestSerialModel::Uart16550,
                transport: GuestSerialTransport::Port {
                    base: 0x3f8,
                    length: 8,
                },
                irq: 4,
                clock_hz: 1_843_200,
            }
        );
        assert_eq!(
            aarch64_profile(1).serial,
            GuestSerialProfile {
                model: GuestSerialModel::Pl011,
                transport: GuestSerialTransport::Mmio {
                    base: 0x0900_0000,
                    length: 0x1000,
                    register_shift: 0,
                    register_width: AccessWidth::Dword,
                },
                irq: 33,
                clock_hz: 24_000_000,
            }
        );
        assert_eq!(
            riscv64_profile(2).serial,
            GuestSerialProfile {
                model: GuestSerialModel::Uart16550,
                transport: GuestSerialTransport::Mmio {
                    base: 0x1000_0000,
                    length: 0x100,
                    register_shift: 0,
                    register_width: AccessWidth::Byte,
                },
                irq: 10,
                clock_hz: 3_686_400,
            }
        );
        assert_eq!(
            loongarch64_profile().serial,
            GuestSerialProfile {
                model: GuestSerialModel::Uart16550,
                transport: GuestSerialTransport::Mmio {
                    base: 0x1fe0_01e0,
                    length: 0x100,
                    register_shift: 0,
                    register_width: AccessWidth::Byte,
                },
                irq: 2,
                clock_hz: 100_000_000,
            }
        );
    }
}
