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

//! VM-owned GIC redistributor register model.

use alloc::vec::Vec;

use ax_kspin::SpinRaw;
use axdevice_base::{
    AccessWidth, BaseDeviceOps, DeviceError, DeviceResult, EmuDeviceType, GuestPhysAddr,
    GuestPhysAddrRange,
};

/// Size of one GIC redistributor frame, including its SGI/PPI frame.
pub const REDISTRIBUTOR_FRAME_SIZE: usize = 0x2_0000;

const GICR_CTLR: usize = 0x0000;
const GICR_IIDR: usize = 0x0004;
const GICR_TYPER: usize = 0x0008;
const GICR_STATUSR: usize = 0x0010;
const GICR_WAKER: usize = 0x0014;
const GICR_PROPBASER: usize = 0x0070;
const GICR_PENDBASER: usize = 0x0078;
const GICR_SYNCR: usize = 0x00c0;
const GICR_PIDR2: usize = 0xffe8;

const GICR_SGI_BASE: usize = 0x1_0000;
const GICR_IGROUPR0: usize = GICR_SGI_BASE + 0x0080;
const GICR_ISENABLER0: usize = GICR_SGI_BASE + 0x0100;
const GICR_ICENABLER0: usize = GICR_SGI_BASE + 0x0180;
const GICR_ISPENDR0: usize = GICR_SGI_BASE + 0x0200;
const GICR_ICPENDR0: usize = GICR_SGI_BASE + 0x0280;
const GICR_ISACTIVER0: usize = GICR_SGI_BASE + 0x0300;
const GICR_ICACTIVER0: usize = GICR_SGI_BASE + 0x0380;
const GICR_IPRIORITYR: usize = GICR_SGI_BASE + 0x0400;
const GICR_IPRIORITYR_END: usize = GICR_IPRIORITYR + 32;
const GICR_ICFGR0: usize = GICR_SGI_BASE + 0x0c00;
const GICR_ICFGR1: usize = GICR_SGI_BASE + 0x0c04;
const GICR_IGRPMODR0: usize = GICR_SGI_BASE + 0x0d00;
const GICR_NSACR: usize = GICR_SGI_BASE + 0x0e00;

#[derive(Clone)]
struct RedistributorState {
    control: u32,
    waker: u32,
    property_base: u64,
    pending_base: u64,
    group: u32,
    enabled: u32,
    pending: u32,
    active: u32,
    priorities: [u8; 32],
    configuration: [u32; 2],
    group_modifier: u32,
}

impl RedistributorState {
    const fn new() -> Self {
        Self {
            control: 0,
            waker: 0,
            property_base: 0,
            pending_base: 0,
            group: 0,
            enabled: 0,
            pending: 0,
            active: 0,
            priorities: [0xa0; 32],
            configuration: [0, 0],
            group_modifier: 0,
        }
    }

    fn read(&self, cpu_id: usize, cpu_count: usize, offset: usize, width: AccessWidth) -> usize {
        if let Some(value) = read_scalar(offset, GICR_TYPER, 8, typer(cpu_id, cpu_count), width) {
            return value;
        }
        if let Some(value) = read_scalar(offset, GICR_PROPBASER, 8, self.property_base, width) {
            return value;
        }
        if let Some(value) = read_scalar(offset, GICR_PENDBASER, 8, self.pending_base, width) {
            return value;
        }
        if (GICR_IPRIORITYR..GICR_IPRIORITYR_END).contains(&offset) {
            return read_bytes(&self.priorities, offset - GICR_IPRIORITYR, width.size());
        }

        match offset {
            GICR_CTLR => self.control as usize,
            GICR_IIDR => 0x43b,
            GICR_STATUSR | GICR_SYNCR => 0,
            GICR_WAKER => self.waker as usize,
            GICR_PIDR2 => 0x3b,
            GICR_IGROUPR0 => self.group as usize,
            GICR_ISENABLER0 | GICR_ICENABLER0 => self.enabled as usize,
            GICR_ISPENDR0 | GICR_ICPENDR0 => self.pending as usize,
            GICR_ISACTIVER0 | GICR_ICACTIVER0 => self.active as usize,
            GICR_ICFGR0 => self.configuration[0] as usize,
            GICR_ICFGR1 => self.configuration[1] as usize,
            GICR_IGRPMODR0 => self.group_modifier as usize,
            GICR_NSACR => 0,
            _ => 0,
        }
    }

    fn write(&mut self, offset: usize, width: AccessWidth, value: usize) {
        if write_scalar(
            offset,
            GICR_PROPBASER,
            8,
            &mut self.property_base,
            width,
            value,
        ) || write_scalar(
            offset,
            GICR_PENDBASER,
            8,
            &mut self.pending_base,
            width,
            value,
        ) {
            return;
        }
        if (GICR_IPRIORITYR..GICR_IPRIORITYR_END).contains(&offset) {
            write_bytes(
                &mut self.priorities,
                offset - GICR_IPRIORITYR,
                width.size(),
                value,
            );
            return;
        }

        let value = value as u32;
        match offset {
            GICR_CTLR => self.control = value,
            // The virtual redistributor completes the wake transition
            // synchronously. ProcessorSleep and ChildrenAsleep therefore read
            // back clear after Linux wakes the redistributor.
            GICR_WAKER => self.waker = value & !((1 << 1) | (1 << 2)),
            GICR_IGROUPR0 => self.group = value,
            GICR_ISENABLER0 => self.enabled |= value,
            GICR_ICENABLER0 => self.enabled &= !value,
            GICR_ISPENDR0 => self.pending |= value,
            GICR_ICPENDR0 => self.pending &= !value,
            GICR_ISACTIVER0 => self.active |= value,
            GICR_ICACTIVER0 => self.active &= !value,
            GICR_ICFGR0 => self.configuration[0] = value,
            GICR_ICFGR1 => self.configuration[1] = value,
            GICR_IGRPMODR0 => self.group_modifier = value,
            GICR_IIDR | GICR_TYPER | GICR_STATUSR | GICR_SYNCR | GICR_PIDR2 | GICR_NSACR => {}
            _ => {}
        }
    }
}

/// Virtual GIC redistributor frames for all vCPUs in one VM.
pub struct VirtualRedistributor {
    base: GuestPhysAddr,
    window_size: usize,
    states: SpinRaw<Vec<RedistributorState>>,
}

impl VirtualRedistributor {
    /// Creates one redistributor frame per virtual CPU.
    pub fn new(base: GuestPhysAddr, cpu_count: usize) -> Self {
        let cpu_count = cpu_count.max(1);
        Self {
            base,
            window_size: cpu_count * REDISTRIBUTOR_FRAME_SIZE,
            states: SpinRaw::new(alloc::vec![RedistributorState::new(); cpu_count]),
        }
    }

    /// Creates redistributor frames inside a larger firmware-described window.
    pub fn new_in_window(
        base: GuestPhysAddr,
        cpu_count: usize,
        window_size: usize,
    ) -> DeviceResult<Self> {
        let cpu_count = cpu_count.max(1);
        let required_size = cpu_count
            .checked_mul(REDISTRIBUTOR_FRAME_SIZE)
            .ok_or_else(|| DeviceError::InvalidInput {
                operation: "initialize virtual GIC redistributor",
                detail: alloc::format!("{cpu_count} redistributor frames overflow usize"),
            })?;
        if window_size < required_size {
            return Err(DeviceError::InvalidInput {
                operation: "initialize virtual GIC redistributor",
                detail: alloc::format!(
                    "firmware window {window_size:#x} is smaller than required size \
                     {required_size:#x}"
                ),
            });
        }
        Ok(Self {
            base,
            window_size,
            states: SpinRaw::new(alloc::vec![RedistributorState::new(); cpu_count]),
        })
    }

    /// Returns the full MMIO range size.
    pub fn size(&self) -> usize {
        self.window_size
    }

    fn decode(&self, addr: GuestPhysAddr) -> DeviceResult<(usize, usize)> {
        let relative =
            addr.as_usize()
                .checked_sub(self.base.as_usize())
                .ok_or(DeviceError::OutOfRange {
                    addr: addr.as_usize() as u64,
                })?;
        let cpu_id = relative / REDISTRIBUTOR_FRAME_SIZE;
        let offset = relative % REDISTRIBUTOR_FRAME_SIZE;
        if cpu_id >= self.states.lock().len() {
            return Err(DeviceError::OutOfRange {
                addr: addr.as_usize() as u64,
            });
        }
        Ok((cpu_id, offset))
    }
}

impl BaseDeviceOps<GuestPhysAddrRange> for VirtualRedistributor {
    fn emu_type(&self) -> EmuDeviceType {
        EmuDeviceType::ArmGicRedistributor
    }

    fn address_range(&self) -> GuestPhysAddrRange {
        GuestPhysAddrRange::from_start_size(self.base, self.size())
    }

    fn handle_read(&self, addr: GuestPhysAddr, width: AccessWidth) -> DeviceResult<usize> {
        let (cpu_id, offset) = self.decode(addr)?;
        let states = self.states.lock();
        Ok(states[cpu_id].read(cpu_id, states.len(), offset, width))
    }

    fn handle_write(&self, addr: GuestPhysAddr, width: AccessWidth, value: usize) -> DeviceResult {
        let (cpu_id, offset) = self.decode(addr)?;
        self.states.lock()[cpu_id].write(offset, width, value);
        Ok(())
    }
}

fn typer(cpu_id: usize, cpu_count: usize) -> u64 {
    let affinity = (cpu_id as u64) << 32;
    let processor_number = (cpu_id as u64) << 8;
    let last = u64::from(cpu_id + 1 == cpu_count) << 4;
    affinity | processor_number | last
}

fn read_scalar(
    offset: usize,
    register: usize,
    register_size: usize,
    value: u64,
    width: AccessWidth,
) -> Option<usize> {
    let size = width.size();
    let byte_offset = offset.checked_sub(register)?;
    if byte_offset + size > register_size {
        return None;
    }
    let mask = if size == 8 {
        u64::MAX
    } else {
        (1u64 << (size * 8)) - 1
    };
    Some(((value >> (byte_offset * 8)) & mask) as usize)
}

fn write_scalar(
    offset: usize,
    register: usize,
    register_size: usize,
    target: &mut u64,
    width: AccessWidth,
    value: usize,
) -> bool {
    let size = width.size();
    let Some(byte_offset) = offset.checked_sub(register) else {
        return false;
    };
    if byte_offset + size > register_size {
        return false;
    }
    let field_mask = if size == 8 {
        u64::MAX
    } else {
        (1u64 << (size * 8)) - 1
    };
    let mask = field_mask << (byte_offset * 8);
    *target = (*target & !mask) | (((value as u64) & field_mask) << (byte_offset * 8));
    true
}

fn read_bytes(bytes: &[u8], offset: usize, size: usize) -> usize {
    bytes
        .iter()
        .skip(offset)
        .take(size)
        .enumerate()
        .fold(0usize, |value, (index, byte)| {
            value | (usize::from(*byte) << (index * 8))
        })
}

fn write_bytes(bytes: &mut [u8], offset: usize, size: usize, value: usize) {
    for (index, byte) in bytes.iter_mut().skip(offset).take(size).enumerate() {
        *byte = (value >> (index * 8)) as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(offset: usize) -> GuestPhysAddr {
        GuestPhysAddr::from(0x080a_0000 + offset)
    }

    #[test]
    fn exposes_gicv3_identity_and_one_last_frame() {
        let redistributor = VirtualRedistributor::new(GuestPhysAddr::from(0x080a_0000), 1);

        assert_eq!(
            redistributor
                .handle_read(addr(GICR_PIDR2), AccessWidth::Dword)
                .unwrap()
                & 0xf0,
            0x30
        );
        assert_ne!(
            redistributor
                .handle_read(addr(GICR_TYPER), AccessWidth::Qword)
                .unwrap()
                & (1 << 4),
            0
        );
    }

    #[test]
    fn firmware_window_can_cover_more_than_the_guest_cpu_frames() {
        let redistributor =
            VirtualRedistributor::new_in_window(GuestPhysAddr::from(0xfe68_0000), 1, 0x10_0000)
                .unwrap();

        assert_eq!(redistributor.size(), 0x10_0000);
        assert_eq!(
            redistributor.address_range(),
            GuestPhysAddrRange::from_start_size(GuestPhysAddr::from(0xfe68_0000), 0x10_0000)
        );
    }

    #[test]
    fn wake_enable_and_clear_registers_preserve_state() {
        let redistributor = VirtualRedistributor::new(GuestPhysAddr::from(0x080a_0000), 1);

        redistributor
            .handle_write(addr(GICR_WAKER), AccessWidth::Dword, u32::MAX as usize)
            .unwrap();
        assert_eq!(
            redistributor
                .handle_read(addr(GICR_WAKER), AccessWidth::Dword)
                .unwrap()
                & ((1 << 1) | (1 << 2)),
            0
        );

        redistributor
            .handle_write(addr(GICR_ISENABLER0), AccessWidth::Dword, 0b1010)
            .unwrap();
        redistributor
            .handle_write(addr(GICR_ICENABLER0), AccessWidth::Dword, 0b0010)
            .unwrap();
        assert_eq!(
            redistributor
                .handle_read(addr(GICR_ISENABLER0), AccessWidth::Dword)
                .unwrap(),
            0b1000
        );
    }
}
