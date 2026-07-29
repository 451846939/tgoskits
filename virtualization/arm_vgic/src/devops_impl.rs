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

use axdevice_base::{AccessWidth, BaseDeviceOps, DeviceAddrRange, DeviceResult, EmuDeviceType};
use axvm_types::GuestPhysAddrRange;

use crate::{VgicError, vgic::Vgic};

impl BaseDeviceOps<GuestPhysAddrRange> for Vgic {
    /// Gets the emulator type of the current device.
    ///
    /// This function returns the emulator device type of the current instance. Specifically, it always returns `EmuDeviceType::EmuDeviceTGicdV2`,
    /// indicating that the emulator device is of type `EmuDeviceTGicdV2`.
    ///
    /// # Returns
    /// - Returns an instance of the `EmuDeviceType` enum, representing the specific type of the emulator device.
    fn emu_type(&self) -> EmuDeviceType {
        EmuDeviceType::InterruptController
    }

    /// Returns the address range for the device.
    ///
    /// This function defines the address range accessible to the device, starting from `0x800_0000`,
    /// with a length of `0x10000` (64KB). It is used to specify where the device can read or write in memory.
    ///
    /// # Returns
    /// An `AddrRange` instance representing the address range from `0x800_0000` to `0x800_FFFF`.
    fn address_range(&self) -> GuestPhysAddrRange {
        GuestPhysAddrRange::from_start_size(0x800_0000.into(), 0x10000)
    }

    /// Handles memory read operations.
    ///
    /// Based on the given physical address and read width, performs the corresponding read operation.
    /// Supports reading 1 byte, 2 bytes, and 4 bytes. This function dereferences the provided physical
    /// address and calls the specific read function based on the width parameter.
    ///
    /// Parameters:
    /// - `addr`: The physical address to read from.
    /// - `width`: The width of the data to be read, determining the size of the read operation.
    ///
    /// Returns:
    /// - `DeviceResult<usize>`: The result of the read operation, including any errors and the size of the data read.
    fn handle_read(
        &self,
        addr: <GuestPhysAddrRange as DeviceAddrRange>::Addr,
        width: AccessWidth,
    ) -> DeviceResult<usize> {
        let range = self.address_range();
        if !range.contains(addr) {
            return Err(VgicError::InvalidAccess {
                operation: "read",
                offset: addr.as_usize(),
                width,
            }
            .into());
        }
        let addr = addr.as_usize() - range.start.as_usize();

        Ok(Vgic::handle_read(self, addr, width)?)
    }
    /// Handles write operations of different widths.
    ///
    /// This function performs a write operation based on the given physical address, width, and value.
    /// It first converts the physical address to a `usize` and applies a mask to ensure proper alignment.
    /// Then, depending on the width parameter, it calls the corresponding write handling function.
    ///
    /// Parameters:
    /// - `addr`: The physical address to write to.
    /// - `width`: The byte width of the data to be written (1, 2, 4 for 8-bit, 16-bit, and 32-bit data respectively).
    /// - `val`: The value to be written.
    fn handle_write(
        &self,
        addr: <GuestPhysAddrRange as DeviceAddrRange>::Addr,
        width: AccessWidth,
        val: usize,
    ) -> DeviceResult {
        let range = self.address_range();
        if !range.contains(addr) {
            return Err(VgicError::InvalidAccess {
                operation: "write",
                offset: addr.as_usize(),
                width,
            }
            .into());
        }
        let addr = addr.as_usize() - range.start.as_usize();

        Vgic::handle_write(self, addr, width, val)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use axdevice_base::{AccessWidth, BaseDeviceOps};
    use axvm_types::{GuestPhysAddr, GuestPhysAddrRange};

    use crate::Vgic;

    const GICD_BASE: usize = 0x0800_0000;

    fn read(vgic: &Vgic, offset: usize, width: AccessWidth) -> usize {
        <Vgic as BaseDeviceOps<GuestPhysAddrRange>>::handle_read(
            vgic,
            GuestPhysAddr::from(GICD_BASE + offset),
            width,
        )
        .unwrap()
    }

    fn write(vgic: &Vgic, offset: usize, width: AccessWidth, value: usize) {
        <Vgic as BaseDeviceOps<GuestPhysAddrRange>>::handle_write(
            vgic,
            GuestPhysAddr::from(GICD_BASE + offset),
            width,
            value,
        )
        .unwrap();
    }

    #[test]
    fn full_distributor_window_reaches_gicv3_identity_registers() {
        let vgic = Vgic::new();
        let value = read(&vgic, 0xffe8, AccessWidth::Dword);

        assert_eq!(value & 0xf0, 0x30);
    }

    #[test]
    fn spi_distributor_registers_keep_guest_owned_state() {
        let vgic = Vgic::new();
        let spi_33_bit = 1 << 1;

        write(&vgic, 0x0084, AccessWidth::Dword, spi_33_bit);
        assert_eq!(read(&vgic, 0x0084, AccessWidth::Dword), spi_33_bit);

        write(&vgic, 0x0104, AccessWidth::Dword, spi_33_bit);
        assert_eq!(read(&vgic, 0x0104, AccessWidth::Dword), spi_33_bit);
        assert_eq!(read(&vgic, 0x0184, AccessWidth::Dword), spi_33_bit);
        write(&vgic, 0x0184, AccessWidth::Dword, spi_33_bit);
        assert_eq!(read(&vgic, 0x0104, AccessWidth::Dword), 0);

        write(&vgic, 0x0204, AccessWidth::Dword, spi_33_bit);
        assert_eq!(read(&vgic, 0x0284, AccessWidth::Dword), spi_33_bit);
        write(&vgic, 0x0284, AccessWidth::Dword, spi_33_bit);
        assert_eq!(read(&vgic, 0x0204, AccessWidth::Dword), 0);

        write(&vgic, 0x0304, AccessWidth::Dword, spi_33_bit);
        assert_eq!(read(&vgic, 0x0384, AccessWidth::Dword), spi_33_bit);
        write(&vgic, 0x0384, AccessWidth::Dword, spi_33_bit);
        assert_eq!(read(&vgic, 0x0304, AccessWidth::Dword), 0);

        write(&vgic, 0x0421, AccessWidth::Byte, 0xa0);
        assert_eq!(read(&vgic, 0x0421, AccessWidth::Byte), 0xa0);

        write(&vgic, 0x0c08, AccessWidth::Dword, 0b10 << 2);
        assert_eq!(read(&vgic, 0x0c08, AccessWidth::Dword), 0b10 << 2);
    }

    #[test]
    fn irouter_supports_full_width_guest_affinity_routes() {
        let vgic = Vgic::new();
        let spi_33_irouter = 0x6000 + 33 * 8;
        let route = 0x0000_0001_0000_0002usize;

        write(&vgic, spi_33_irouter, AccessWidth::Qword, route);

        assert_eq!(read(&vgic, spi_33_irouter, AccessWidth::Qword), route);
    }

    #[test]
    fn private_interrupt_distributor_registers_are_raz_wi() {
        let vgic = Vgic::new();

        write(&vgic, 0x0080, AccessWidth::Dword, u32::MAX as usize);
        write(&vgic, 0x0100, AccessWidth::Dword, u32::MAX as usize);
        write(&vgic, 0x0200, AccessWidth::Dword, u32::MAX as usize);
        write(&vgic, 0x0300, AccessWidth::Dword, u32::MAX as usize);
        write(&vgic, 0x0400, AccessWidth::Dword, u32::MAX as usize);
        write(&vgic, 0x0c00, AccessWidth::Dword, u32::MAX as usize);
        write(&vgic, 0x6000, AccessWidth::Qword, usize::MAX);

        assert_eq!(read(&vgic, 0x0080, AccessWidth::Dword), 0);
        assert_eq!(read(&vgic, 0x0100, AccessWidth::Dword), 0);
        assert_eq!(read(&vgic, 0x0200, AccessWidth::Dword), 0);
        assert_eq!(read(&vgic, 0x0300, AccessWidth::Dword), 0);
        assert_eq!(read(&vgic, 0x0400, AccessWidth::Dword), 0);
        assert_eq!(read(&vgic, 0x0c00, AccessWidth::Dword), 0);
        assert_eq!(read(&vgic, 0x6000, AccessWidth::Qword), 0);
    }
}
