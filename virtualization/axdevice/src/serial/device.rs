//! Unified bus adapters for virtual UART cores.

use alloc::{boxed::Box, sync::Arc};
use core::any::Any;

use axdevice_base::{
    AccessWidth, BusAccess, BusKind, BusResponse, Device, DeviceError, InterruptTriggerMode,
    IrqLine, Resource,
};

use crate::{
    DeviceBundle, DeviceManagerResult, DeviceRegistration, PollableDeviceOps,
    serial::{Pl011, SerialBackend, Uart16550},
};

struct Uart16550PortDevice {
    core: Uart16550,
    base: u16,
    resources: Box<[Resource]>,
}

impl Uart16550PortDevice {
    fn new(
        base: u16,
        length: u16,
        irq_id: usize,
        backend: Arc<dyn SerialBackend>,
        irq: IrqLine,
    ) -> Self {
        Self {
            core: Uart16550::new(backend, irq),
            base,
            resources: alloc::vec![
                Resource::PortRange { base, size: length },
                irq_resource(irq_id),
            ]
            .into_boxed_slice(),
        }
    }
}

impl Device for Uart16550PortDevice {
    fn name(&self) -> &str {
        "uart16550-port"
    }

    fn resources(&self) -> &[Resource] {
        &self.resources
    }

    fn handle(&self, access: &BusAccess) -> Result<BusResponse, DeviceError> {
        if access.kind != BusKind::Port {
            return Err(DeviceError::OutOfRange { addr: access.addr });
        }
        let offset = u16::try_from(access.addr)
            .ok()
            .and_then(|port| port.checked_sub(self.base))
            .ok_or(DeviceError::OutOfRange { addr: access.addr })? as usize;
        handle_16550(&self.core, offset, AccessWidth::Byte, access)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl PollableDeviceOps for Uart16550PortDevice {
    fn poll(&self, _now_ns: u64) -> DeviceManagerResult {
        self.core.poll().map_err(Into::into)
    }
}

struct Uart16550MmioDevice {
    core: Uart16550,
    base: u64,
    register_shift: u8,
    register_width: AccessWidth,
    resources: Box<[Resource]>,
}

impl Uart16550MmioDevice {
    fn new(
        base: usize,
        length: usize,
        register_shift: u8,
        register_width: AccessWidth,
        irq_id: usize,
        backend: Arc<dyn SerialBackend>,
        irq: IrqLine,
    ) -> Self {
        Self {
            core: Uart16550::new(backend, irq),
            base: base as u64,
            register_shift,
            register_width,
            resources: alloc::vec![
                Resource::MmioRange {
                    base: base as u64,
                    size: length as u64,
                },
                irq_resource(irq_id),
            ]
            .into_boxed_slice(),
        }
    }
}

impl Device for Uart16550MmioDevice {
    fn name(&self) -> &str {
        "uart16550-mmio"
    }

    fn resources(&self) -> &[Resource] {
        &self.resources
    }

    fn handle(&self, access: &BusAccess) -> Result<BusResponse, DeviceError> {
        if access.kind != BusKind::Mmio {
            return Err(DeviceError::OutOfRange { addr: access.addr });
        }
        let offset = access
            .addr
            .checked_sub(self.base)
            .ok_or(DeviceError::OutOfRange { addr: access.addr })?;
        let register = usize::try_from(offset >> self.register_shift)
            .map_err(|_| DeviceError::OutOfRange { addr: access.addr })?;
        handle_16550(&self.core, register, self.register_width, access)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl PollableDeviceOps for Uart16550MmioDevice {
    fn poll(&self, _now_ns: u64) -> DeviceManagerResult {
        self.core.poll().map_err(Into::into)
    }
}

struct Pl011MmioDevice {
    core: Pl011,
    base: u64,
    resources: Box<[Resource]>,
}

impl Pl011MmioDevice {
    fn new(
        base: usize,
        length: usize,
        irq_id: usize,
        backend: Arc<dyn SerialBackend>,
        irq: IrqLine,
    ) -> Self {
        Self {
            core: Pl011::new(backend, irq),
            base: base as u64,
            resources: alloc::vec![
                Resource::MmioRange {
                    base: base as u64,
                    size: length as u64,
                },
                irq_resource(irq_id),
            ]
            .into_boxed_slice(),
        }
    }
}

impl Device for Pl011MmioDevice {
    fn name(&self) -> &str {
        "pl011"
    }

    fn resources(&self) -> &[Resource] {
        &self.resources
    }

    fn handle(&self, access: &BusAccess) -> Result<BusResponse, DeviceError> {
        if access.kind != BusKind::Mmio {
            return Err(DeviceError::OutOfRange { addr: access.addr });
        }
        let offset = access
            .addr
            .checked_sub(self.base)
            .ok_or(DeviceError::OutOfRange { addr: access.addr })?;
        let offset =
            usize::try_from(offset).map_err(|_| DeviceError::OutOfRange { addr: access.addr })?;
        if access.is_read {
            self.core
                .read(offset, access.width)
                .map(|value| BusResponse::Read { value })
        } else {
            self.core.write(offset, access.width, access.data)?;
            Ok(BusResponse::Write)
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl PollableDeviceOps for Pl011MmioDevice {
    fn poll(&self, _now_ns: u64) -> DeviceManagerResult {
        self.core.poll().map_err(Into::into)
    }
}

/// Builds a port-mapped 16550 UART bundle.
pub fn build_16550_port(
    base: u16,
    length: u16,
    irq_id: usize,
    backend: Arc<dyn SerialBackend>,
    irq: IrqLine,
) -> DeviceBundle {
    bundle(Arc::new(Uart16550PortDevice::new(
        base, length, irq_id, backend, irq,
    )))
}

/// Builds a memory-mapped 16550 UART bundle.
pub fn build_16550_mmio(
    base: usize,
    length: usize,
    register_shift: u8,
    register_width: AccessWidth,
    irq_id: usize,
    backend: Arc<dyn SerialBackend>,
    irq: IrqLine,
) -> DeviceBundle {
    bundle(Arc::new(Uart16550MmioDevice::new(
        base,
        length,
        register_shift,
        register_width,
        irq_id,
        backend,
        irq,
    )))
}

/// Builds a memory-mapped PL011 UART bundle.
pub fn build_pl011_mmio(
    base: usize,
    length: usize,
    irq_id: usize,
    backend: Arc<dyn SerialBackend>,
    irq: IrqLine,
) -> DeviceBundle {
    bundle(Arc::new(Pl011MmioDevice::new(
        base, length, irq_id, backend, irq,
    )))
}

fn bundle<D>(device: Arc<D>) -> DeviceBundle
where
    D: Device + PollableDeviceOps + 'static,
{
    DeviceBundle::new()
        .with_registration(DeviceRegistration::Device(device.clone()))
        .with_registration(DeviceRegistration::Pollable(device))
}

fn handle_16550(
    core: &Uart16550,
    register: usize,
    register_width: AccessWidth,
    access: &BusAccess,
) -> Result<BusResponse, DeviceError> {
    if access.width != register_width {
        return Err(DeviceError::InvalidWidth {
            expected: register_width,
            actual: access.width,
        });
    }
    if access.is_read {
        core.read(register, AccessWidth::Byte)
            .map(|value| BusResponse::Read { value })
    } else {
        core.write(register, AccessWidth::Byte, access.data)?;
        Ok(BusResponse::Write)
    }
}

fn irq_resource(irq_id: usize) -> Resource {
    Resource::IrqLine {
        line: u32::try_from(irq_id).expect("machine-profile IRQ must fit u32"),
        trigger: InterruptTriggerMode::LevelTriggered,
    }
}

#[allow(dead_code)]
fn _assert_access_width_is_exhaustive(width: AccessWidth) {
    match width {
        AccessWidth::Byte | AccessWidth::Word | AccessWidth::Dword | AccessWidth::Qword => {}
    }
}

#[cfg(test)]
mod tests {
    use alloc::{sync::Arc, vec::Vec};
    use std::sync::Mutex;

    use axdevice_base::{IrqLineId, IrqResult, IrqSink};

    use super::*;

    #[derive(Debug, Default)]
    struct RecordingBackend {
        output: Mutex<Vec<u8>>,
    }

    impl SerialBackend for RecordingBackend {
        fn write(&self, bytes: &[u8]) {
            self.output.lock().unwrap().extend_from_slice(bytes);
        }

        fn read(&self, _buffer: &mut [u8]) -> usize {
            0
        }
    }

    struct NoopIrqSink;

    impl IrqSink for NoopIrqSink {
        fn set_level(&self, _line: IrqLineId, _asserted: bool) -> IrqResult {
            Ok(())
        }

        fn pulse(&self, _line: IrqLineId) -> IrqResult {
            Ok(())
        }
    }

    #[test]
    fn mmio_16550_accepts_dword_access_for_four_byte_registers() {
        let backend = Arc::new(RecordingBackend::default());
        let irq = IrqLine::new(
            IrqLineId(365),
            InterruptTriggerMode::LevelTriggered,
            Arc::new(NoopIrqSink),
        );
        let device = Uart16550MmioDevice::new(
            0xfeb5_0000,
            0x100,
            2,
            AccessWidth::Dword,
            365,
            backend.clone(),
            irq,
        );

        let response = device
            .handle(&BusAccess {
                kind: BusKind::Mmio,
                is_read: false,
                addr: 0xfeb5_0000,
                width: AccessWidth::Dword,
                data: b'A' as u64,
            })
            .unwrap();

        assert!(matches!(response, BusResponse::Write));
        assert_eq!(backend.output.lock().unwrap().as_slice(), b"A");
    }

    #[test]
    fn mmio_16550_ignores_unimplemented_registers_inside_its_window() {
        let irq = IrqLine::new(
            IrqLineId(365),
            InterruptTriggerMode::LevelTriggered,
            Arc::new(NoopIrqSink),
        );
        let device = Uart16550MmioDevice::new(
            0xfeb5_0000,
            0x100,
            2,
            AccessWidth::Dword,
            365,
            Arc::new(RecordingBackend::default()),
            irq,
        );

        let write = device
            .handle(&BusAccess {
                kind: BusKind::Mmio,
                is_read: false,
                addr: 0xfeb5_0088,
                width: AccessWidth::Dword,
                data: 1,
            })
            .unwrap();
        let read = device
            .handle(&BusAccess {
                kind: BusKind::Mmio,
                is_read: true,
                addr: 0xfeb5_0088,
                width: AccessWidth::Dword,
                data: 0,
            })
            .unwrap();

        assert!(matches!(write, BusResponse::Write));
        assert!(matches!(read, BusResponse::Read { value: 0 }));
    }
}
