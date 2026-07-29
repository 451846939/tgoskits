use alloc::vec::Vec;

use axdevice::{FwCfgInterruptConfig, FwCfgPciConfig};
use axvmconfig::GuestConfig;

use super::{
    FirmwareDevices, FlashDevice, GedDevice, GuestPlatform, InterruptTopology, IrqMmioDevice,
    MemoryRegion, MmioRegion, PciHost, SerialDevice,
};

pub struct GuestPlatformBuilder {
    ram_regions: Vec<MemoryRegion>,
    fw_cfg: MmioRegion,
    pci: Option<PciHost>,
    firmware_devices: Option<FirmwareDevices>,
    irq_routes: Vec<GuestIrqRoute>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct GuestIrqRoute {
    pub physical_irq: usize,
    pub guest_vector: usize,
}

impl GuestPlatformBuilder {
    pub fn new(ram_regions: Vec<MemoryRegion>, config: &GuestConfig) -> Self {
        Self {
            ram_regions,
            fw_cfg: fw_cfg_region(config),
            pci: None,
            firmware_devices: None,
            irq_routes: Vec::new(),
        }
    }

    pub fn apply_host_acpi(mut self) -> Self {
        if let Some(result) = ax_driver::probe::acpi::with_acpi(host_acpi_resources) {
            match result {
                Ok(resources) => self.apply_host_resources(resources),
                Err(err) => warn!("failed to collect LoongArch host ACPI resources: {err:?}"),
            }
        }
        self
    }

    pub fn build(self) -> GuestPlatform {
        let defaults = QemuVirtDefaults::new();
        let pci = self.pci.unwrap_or(defaults.pci);

        GuestPlatform {
            ram_regions: self.ram_regions,
            serial: defaults.serial,
            pci,
            interrupt: defaults.interrupt,
            fw_cfg: self.fw_cfg,
            firmware_devices: self.firmware_devices.unwrap_or(defaults.firmware_devices),
            irq_routes: if self.irq_routes.is_empty() {
                defaults.irq_routes
            } else {
                self.irq_routes
            },
        }
    }

    fn apply_host_resources(&mut self, resources: HostResources) {
        if let Some(pci) = resources.pci {
            self.pci = Some(pci);
        }
        if let Some(firmware_devices) = resources.firmware_devices {
            self.firmware_devices = Some(firmware_devices);
        }
        self.irq_routes = resources.irq_routes;
    }
}

struct HostResources {
    pci: Option<PciHost>,
    firmware_devices: Option<FirmwareDevices>,
    irq_routes: Vec<GuestIrqRoute>,
}

fn host_acpi_resources(
    acpi: &ax_driver::probe::acpi::System,
) -> axvm_types::VmBackendResult<HostResources> {
    let defaults = QemuVirtDefaults::new();

    let pci = acpi.pci_ecam_regions().first().map(|ecam| PciHost {
        ecam: MmioRegion {
            base: ecam.base_address,
            size: ecam.size() as u64,
        },
        mmio: defaults.pci.mmio,
        io_base: defaults.pci.io_base,
        io_size: defaults.pci.io_size,
        intx_base: defaults.pci.intx_base,
    });

    let irq_routes = guest_irq_routes(&pci);
    let firmware_devices = Some(find_firmware_devices(acpi, defaults.firmware_devices));

    Ok(HostResources {
        pci,
        firmware_devices,
        irq_routes,
    })
}

fn find_firmware_devices(
    acpi: &ax_driver::probe::acpi::System,
    mut devices: FirmwareDevices,
) -> FirmwareDevices {
    if let Some(rtc) = find_rtc(acpi) {
        devices.rtc = rtc;
    }
    devices
}

fn find_rtc(acpi: &ax_driver::probe::acpi::System) -> Option<IrqMmioDevice> {
    let devices = acpi.resource_devices().ok()?;
    devices.into_iter().find_map(|device| {
        let is_rtc = device.hid.as_deref() == Some("LOON0001")
            || device.cids.iter().any(|cid| cid == "LOON0001")
            || device.path.contains("RTC");
        if !is_rtc {
            return None;
        }
        let range = device.memory_ranges.first()?;
        let irq = device
            .irq_routes
            .first()
            .map(|route| u32::from(route.controller_input))
            .unwrap_or(defaults_rtc_irq());
        Some(IrqMmioDevice {
            mmio: MmioRegion {
                base: range.base,
                size: range.size,
            },
            irq,
        })
    })
}

fn defaults_rtc_irq() -> u32 {
    6
}

fn guest_irq_routes(pci: &Option<PciHost>) -> Vec<GuestIrqRoute> {
    let defaults = QemuVirtDefaults::new();
    let pci = pci.unwrap_or(defaults.pci);

    (0..4)
        .map(|idx| GuestIrqRoute {
            physical_irq: pci.intx_base as usize + idx,
            guest_vector: pci.intx_base as usize + idx,
        })
        .collect()
}

fn fw_cfg_region(config: &GuestConfig) -> MmioRegion {
    if let Some(fw_cfg) = crate::machine::current_machine_profile(config.base.cpu_num)
        .emulated_devices
        .into_iter()
        .find(|device| device.emu_type == axvm_types::EmulatedDeviceType::FwCfg)
    {
        return MmioRegion {
            base: fw_cfg.base_gpa as u64,
            size: fw_cfg.length as u64,
        };
    }

    QemuVirtDefaults::new().fw_cfg
}

struct QemuVirtDefaults {
    serial: SerialDevice,
    pci: PciHost,
    interrupt: InterruptTopology,
    fw_cfg: MmioRegion,
    firmware_devices: FirmwareDevices,
    irq_routes: Vec<GuestIrqRoute>,
}

impl QemuVirtDefaults {
    fn new() -> Self {
        let machine = crate::machine::current_machine_profile(1);
        let serial_profile = machine.serial;
        let crate::machine::GuestSerialTransport::Mmio {
            base: serial_base,
            length: serial_size,
            ..
        } = serial_profile.transport
        else {
            unreachable!("LoongArch machine serial must be MMIO");
        };
        let fw_cfg = machine
            .emulated_devices
            .iter()
            .find(|device| device.emu_type == axvm_types::EmulatedDeviceType::FwCfg)
            .expect("LoongArch machine must contain fw_cfg");
        let pch_pic = machine
            .emulated_devices
            .iter()
            .find(|device| device.emu_type == axvm_types::EmulatedDeviceType::LoongArchPchPic)
            .expect("LoongArch machine must contain a PCH-PIC");
        let serial = SerialDevice {
            mmio: MmioRegion {
                base: serial_base as u64,
                size: serial_size as u64,
            },
            irq: serial_profile.irq as u32,
            clock_hz: serial_profile.clock_hz,
            baud: 115_200,
        };
        let pci = PciHost {
            ecam: MmioRegion {
                base: FwCfgPciConfig::default().ecam_base,
                size: FwCfgPciConfig::default().ecam_size,
            },
            mmio: MmioRegion {
                base: FwCfgPciConfig::default().mmio_base,
                size: FwCfgPciConfig::default().mmio_size,
            },
            io_base: FwCfgPciConfig::default().io_base,
            io_size: u64::from(FwCfgPciConfig::default().io_size),
            intx_base: 16,
        };
        let interrupt = InterruptTopology {
            eiointc_irq: FwCfgInterruptConfig::default().eiointc_irq as u32,
            pch_pic: MmioRegion {
                base: pch_pic.base_gpa as u64,
                size: pch_pic.length as u64,
            },
            pch_pic_gsi_base: 0,
            pch_msi: MmioRegion {
                base: FwCfgInterruptConfig::default().pch_msi_base,
                size: 0x8,
            },
            pch_msi_start: 0x20,
            pch_msi_count: 0xe0,
            acpi_gsi_base: u32::from(FwCfgInterruptConfig::default().pch_pic_gsi_base),
            acpi_msi_start: FwCfgInterruptConfig::default().pch_msi_start,
            acpi_msi_count: FwCfgInterruptConfig::default().pch_msi_count,
        };
        let irq_routes = Vec::from([
            GuestIrqRoute {
                physical_irq: pci.intx_base as usize,
                guest_vector: pci.intx_base as usize,
            },
            GuestIrqRoute {
                physical_irq: pci.intx_base as usize + 1,
                guest_vector: pci.intx_base as usize + 1,
            },
            GuestIrqRoute {
                physical_irq: pci.intx_base as usize + 2,
                guest_vector: pci.intx_base as usize + 2,
            },
            GuestIrqRoute {
                physical_irq: pci.intx_base as usize + 3,
                guest_vector: pci.intx_base as usize + 3,
            },
        ]);
        Self {
            serial,
            pci,
            interrupt,
            fw_cfg: MmioRegion {
                base: fw_cfg.base_gpa as u64,
                size: fw_cfg.length as u64,
            },
            firmware_devices: FirmwareDevices {
                rtc: IrqMmioDevice {
                    mmio: MmioRegion {
                        base: 0x100d_0100,
                        size: 0x100,
                    },
                    irq: defaults_rtc_irq(),
                },
                flash: FlashDevice {
                    banks: [
                        MmioRegion {
                            base: 0x1c00_0000,
                            size: 0x0100_0000,
                        },
                        MmioRegion {
                            base: 0x1d00_0000,
                            size: 0x0100_0000,
                        },
                    ],
                    bank_width: 4,
                },
                ged: GedDevice {
                    mmio: MmioRegion {
                        base: 0x100e_001c,
                        size: 3,
                    },
                    poweroff_offset: 0,
                    poweroff_value: 0x34,
                    reboot_offset: 2,
                    reboot_value: 0x42,
                },
            },
            irq_routes,
        }
    }
}
