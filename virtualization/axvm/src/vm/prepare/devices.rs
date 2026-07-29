//! Device construction for VM preparation.

use axdevice::{
    AxVmDeviceConfig, AxVmDevices, DeviceBuildContext, DeviceFactoryRegistry, IrqResolver,
    build_16550_mmio, build_16550_port, build_pl011_mmio,
};
use axdevice_base::InterruptTriggerMode;
use axvm_types::EmulatedDeviceType;

use super::super::{AxVM, AxVMResources};
use crate::{
    AxVmResult,
    irq::InterruptFabric,
    machine::{GuestSerialModel, GuestSerialTransport},
};

pub(crate) struct PreparedDevices {
    pub(crate) devices: AxVmDevices,
}

impl PreparedDevices {
    pub(crate) fn build_common(
        resources: &AxVMResources,
        factories: &DeviceFactoryRegistry,
        interrupt_fabric: &InterruptFabric,
    ) -> AxVmResult<Self> {
        let build_context = DeviceBuildContext::new(interrupt_fabric);
        let emu_configs = resources
            .config
            .emu_devices()
            .iter()
            .filter(|config| config.emu_type != EmulatedDeviceType::Console)
            .cloned()
            .collect();
        let mut devices = AxVmDevices::build_with_factories(
            AxVmDeviceConfig { emu_configs },
            factories,
            &build_context,
        )?;
        register_serial_device(resources, interrupt_fabric, &mut devices)?;

        Ok(Self { devices })
    }

    pub(crate) fn register_special_devices(&mut self, vm: &AxVM) -> AxVmResult {
        vm.add_special_emulated_devices(&mut self.devices)
    }

    pub(crate) const fn devices(&self) -> &AxVmDevices {
        &self.devices
    }

    pub(crate) fn into_inner(self) -> AxVmDevices {
        self.devices
    }
}

fn register_serial_device(
    resources: &AxVMResources,
    interrupt_fabric: &InterruptFabric,
    devices: &mut AxVmDevices,
) -> AxVmResult {
    let mut serial_configs = resources
        .config
        .emu_devices()
        .iter()
        .filter(|config| config.emu_type == EmulatedDeviceType::Console);
    let config = serial_configs
        .next()
        .ok_or_else(|| crate::AxVmError::invalid_config("machine profile has no serial device"))?;
    if serial_configs.next().is_some() {
        return Err(crate::AxVmError::invalid_config(
            "machine profile has more than one serial device",
        ));
    }

    let irq = interrupt_fabric.resolve_irq(config.irq_id, InterruptTriggerMode::LevelTriggered)?;
    let backend = resources.config.serial_backend();
    let serial = crate::machine::current_machine_profile(1).serial;
    let (profile_base, profile_length) = match serial.transport {
        GuestSerialTransport::Port { base, length } => (usize::from(base), usize::from(length)),
        GuestSerialTransport::Mmio { base, length, .. } => (base, length),
    };
    if (config.base_gpa, config.length, config.irq_id) != (profile_base, profile_length, serial.irq)
    {
        return Err(crate::AxVmError::invalid_config(
            "serial descriptor does not match the machine profile",
        ));
    }

    let bundle = match (serial.model, serial.transport) {
        (GuestSerialModel::Uart16550, GuestSerialTransport::Port { base, length }) => {
            build_16550_port(base, length, serial.irq, backend, irq)
        }
        (
            GuestSerialModel::Uart16550,
            GuestSerialTransport::Mmio {
                base,
                length,
                register_shift,
            },
        ) => build_16550_mmio(base, length, register_shift, serial.irq, backend, irq),
        (GuestSerialModel::Pl011, GuestSerialTransport::Mmio { base, length, .. }) => {
            build_pl011_mmio(base, length, serial.irq, backend, irq)
        }
        (GuestSerialModel::Pl011, GuestSerialTransport::Port { .. }) => {
            return Err(crate::AxVmError::invalid_config(
                "PL011 machine serial cannot use port I/O",
            ));
        }
    };

    devices.register_bundle(bundle)?;
    Ok(())
}
