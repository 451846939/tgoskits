use alloc::{boxed::Box, format, string::String, sync::Arc, vec};

use axdevice_base::{
    AccessWidth, BusAccess, BusKind, BusResponse, Device, DeviceAccess, DeviceError, DeviceResult,
    Resource,
};
use axvm_types::{EmulatedDeviceConfig, EmulatedDeviceType, GuestPhysAddr};

use crate::{DeviceBuildContext, DeviceBundle, DeviceFactory, DeviceManagerResult};

const PCI_CONFIG_SPACE_SIZE: usize = 256;
const PCI_ECAM_FUNCTION_SIZE: usize = 4096;

const DEFAULT_BUS: u8 = 0;
const DEFAULT_DEVICE: u8 = 5;
const DEFAULT_FUNCTION: u8 = 0;
const DEFAULT_VENDOR_ID: u16 = 0xaaaa;
const DEFAULT_DEVICE_ID: u16 = 0x0001;

/// A minimal PCI endpoint exposed through a virtual ECAM host bridge.
#[derive(Clone, Copy, Debug)]
pub struct VirtualPciEndpointConfig {
    /// PCI bus number.
    pub bus: u8,
    /// PCI device number.
    pub device: u8,
    /// PCI function number.
    pub function: u8,
    /// PCI vendor ID.
    pub vendor_id: u16,
    /// PCI device ID.
    pub device_id: u16,
}

impl Default for VirtualPciEndpointConfig {
    fn default() -> Self {
        Self {
            bus: DEFAULT_BUS,
            device: DEFAULT_DEVICE,
            function: DEFAULT_FUNCTION,
            vendor_id: DEFAULT_VENDOR_ID,
            device_id: DEFAULT_DEVICE_ID,
        }
    }
}

impl VirtualPciEndpointConfig {
    fn from_cfg_list(cfg_list: &[usize]) -> DeviceManagerResult<Self> {
        let mut config = Self::default();
        if let Some(value) = cfg_list.first().copied() {
            config.bus = checked_u8(value, "PCI bus")?;
        }
        if let Some(value) = cfg_list.get(1).copied() {
            config.device = checked_u8(value, "PCI device")?;
            if config.device >= 32 {
                return invalid_vpci_config("PCI device must be less than 32");
            }
        }
        if let Some(value) = cfg_list.get(2).copied() {
            config.function = checked_u8(value, "PCI function")?;
            if config.function >= 8 {
                return invalid_vpci_config("PCI function must be less than 8");
            }
        }
        if let Some(value) = cfg_list.get(3).copied() {
            config.vendor_id = checked_u16(value, "PCI vendor ID")?;
        }
        if let Some(value) = cfg_list.get(4).copied() {
            config.device_id = checked_u16(value, "PCI device ID")?;
        }
        Ok(config)
    }
}

/// Minimal virtual PCI host bridge backed by ECAM MMIO configuration space.
pub struct VirtualPciHost {
    name: String,
    base: GuestPhysAddr,
    size: usize,
    endpoint: VirtualPciEndpointConfig,
    resources: Box<[Resource]>,
    config_space: [u8; PCI_CONFIG_SPACE_SIZE],
}

impl VirtualPciHost {
    /// Creates a virtual PCI host bridge with one dummy endpoint.
    pub fn new(
        name: String,
        base: GuestPhysAddr,
        size: usize,
        endpoint: VirtualPciEndpointConfig,
    ) -> DeviceManagerResult<Self> {
        if size < PCI_ECAM_FUNCTION_SIZE {
            return invalid_vpci_config("ECAM range must be at least one 4 KiB function");
        }

        let mut config_space = [0u8; PCI_CONFIG_SPACE_SIZE];
        config_space[0x00..0x02].copy_from_slice(&endpoint.vendor_id.to_le_bytes());
        config_space[0x02..0x04].copy_from_slice(&endpoint.device_id.to_le_bytes());
        config_space[0x08] = 0x00; // revision id
        config_space[0x09] = 0x00; // programming interface
        config_space[0x0a] = 0x00; // subclass
        config_space[0x0b] = 0xff; // vendor-specific class
        config_space[0x0e] = 0x00; // endpoint header

        let resources = vec![Resource::MmioRange {
            base: base.as_usize() as u64,
            size: size as u64,
        }]
        .into_boxed_slice();

        Ok(Self {
            name,
            base,
            size,
            endpoint,
            resources,
            config_space,
        })
    }

    fn contains(&self, addr: GuestPhysAddr) -> bool {
        let base = self.base.as_usize();
        let end = base.saturating_add(self.size);
        let addr = addr.as_usize();
        addr >= base && addr < end
    }

    fn decode_ecam(&self, addr: GuestPhysAddr) -> Option<(u8, u8, u8, usize)> {
        if !self.contains(addr) {
            return None;
        }
        let offset = addr.as_usize() - self.base.as_usize();
        let bus = ((offset >> 20) & 0xff) as u8;
        let device = ((offset >> 15) & 0x1f) as u8;
        let function = ((offset >> 12) & 0x07) as u8;
        let register = offset & 0xfff;
        Some((bus, device, function, register))
    }

    fn selected_endpoint(&self, addr: GuestPhysAddr) -> Option<usize> {
        let (bus, device, function, register) = self.decode_ecam(addr)?;
        (bus == self.endpoint.bus
            && device == self.endpoint.device
            && function == self.endpoint.function
            && register < PCI_CONFIG_SPACE_SIZE)
            .then_some(register)
    }

    fn absent_value(width: AccessWidth) -> u64 {
        match width {
            AccessWidth::Byte => u8::MAX as u64,
            AccessWidth::Word => u16::MAX as u64,
            AccessWidth::Dword => u32::MAX as u64,
            AccessWidth::Qword => u64::MAX,
        }
    }

    fn read_config(&self, addr: GuestPhysAddr, width: AccessWidth) -> DeviceResult<u64> {
        let Some(register) = self.selected_endpoint(addr) else {
            return Ok(Self::absent_value(width));
        };

        let mut value = 0u64;
        for byte in 0..width.size() {
            let Some(data) = self.config_space.get(register + byte) else {
                break;
            };
            value |= (*data as u64) << (byte * 8);
        }
        Ok(value)
    }

    fn write_config(&self, addr: GuestPhysAddr, width: AccessWidth, _value: u64) -> DeviceResult {
        if !self.contains(addr) {
            return Err(DeviceError::OutOfRange {
                addr: addr.as_usize() as u64,
            });
        }
        if self.selected_endpoint(addr).is_none() {
            return Ok(());
        }
        if width.size() > core::mem::size_of::<u64>() {
            return Err(DeviceError::InvalidInput {
                operation: "write virtual pci config space",
                detail: format!("unsupported access width {width:?}"),
            });
        }
        Ok(())
    }
}

impl Device for VirtualPciHost {
    fn name(&self) -> &str {
        &self.name
    }

    fn resources(&self) -> &[Resource] {
        &self.resources
    }

    fn access(
        &self,
        access: &BusAccess,
        _context: &mut dyn DeviceAccess,
    ) -> DeviceResult<BusResponse> {
        if access.kind != BusKind::Mmio {
            return Err(DeviceError::NotFound);
        }

        let addr = GuestPhysAddr::from(access.addr as usize);
        if access.is_read {
            Ok(BusResponse::Read {
                value: self.read_config(addr, access.width)?,
            })
        } else {
            self.write_config(addr, access.width, access.data)?;
            Ok(BusResponse::Write)
        }
    }
}

/// Builds minimal virtual PCI host bridge devices from VM configuration.
pub struct VirtualPciHostFactory;

impl DeviceFactory for VirtualPciHostFactory {
    fn device_type(&self) -> EmulatedDeviceType {
        EmulatedDeviceType::VirtualPciHost
    }

    fn build(
        &self,
        config: &EmulatedDeviceConfig,
        _context: &DeviceBuildContext<'_>,
    ) -> DeviceManagerResult<DeviceBundle> {
        let endpoint = VirtualPciEndpointConfig::from_cfg_list(&config.cfg_list)?;
        let device = VirtualPciHost::new(
            config.name.clone(),
            GuestPhysAddr::from(config.base_gpa),
            config.length,
            endpoint,
        )?;
        let mut bundle = DeviceBundle::new();
        bundle.add_device(Arc::new(device));
        Ok(bundle)
    }
}

fn checked_u8(value: usize, field: &'static str) -> DeviceManagerResult<u8> {
    u8::try_from(value).map_err(|_| crate::DeviceManagerError::InvalidInput {
        operation: "build virtual pci host",
        detail: format!("{field} value {value:#x} does not fit in u8"),
    })
}

fn checked_u16(value: usize, field: &'static str) -> DeviceManagerResult<u16> {
    u16::try_from(value).map_err(|_| crate::DeviceManagerError::InvalidInput {
        operation: "build virtual pci host",
        detail: format!("{field} value {value:#x} does not fit in u16"),
    })
}

fn invalid_vpci_config<T>(detail: &'static str) -> DeviceManagerResult<T> {
    Err(crate::DeviceManagerError::InvalidInput {
        operation: "build virtual pci host",
        detail: detail.into(),
    })
}
