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

use alloc::vec::Vec;

use crate::{
    consts::{PPI_ID_MAX, SPI_ID_MAX},
    interrupt::{InterruptStatus, TriggerMode, VgicInt},
    registers::{
        GICD_CTLR, GICD_ICACTIVER, GICD_ICENABLER, GICD_ICFGR, GICD_ICPENDR, GICD_IGROUPR,
        GICD_IIDR, GICD_IPRIORITYR, GICD_IROUTER, GICD_ISACTIVER, GICD_ISENABLER, GICD_ISPENDR,
        GICD_ITARGETSR, GICD_PIDR2, GICD_STATUSR, GICD_TYPER, GICD_TYPER2,
    },
};

const GICD_CTLR_WRITABLE_MASK: u32 = 0x0000_00f7;
const GICD_TYPER_ID_BITS_SHIFT: u32 = 19;
const GICD_ID_BITS_VALUE: u32 = usize::BITS - (SPI_ID_MAX - 1).leading_zeros() - 1;
const GICD_TYPER_VALUE: u32 =
    (SPI_ID_MAX / 32 - 1) as u32 | (GICD_ID_BITS_VALUE << GICD_TYPER_ID_BITS_SHIFT);
const GICD_IIDR_VALUE: u32 = 0x0102_043b;

pub struct Vgicd {
    ctrlr: u32,
    groups: [bool; SPI_ID_MAX],
    interrupt: [VgicInt; SPI_ID_MAX],
    line_asserted: [bool; SPI_ID_MAX],
    routes: [u64; SPI_ID_MAX],
}

impl Vgicd {
    pub fn new() -> Self {
        let mut gic_int = [VgicInt::new(0, 0); SPI_ID_MAX];
        for (idx, item) in gic_int.iter_mut().enumerate() {
            *item = VgicInt::new(idx as u32, 0);
            if idx >= PPI_ID_MAX {
                item.set_trigger_mode(TriggerMode::Level);
            }
        }
        Self {
            ctrlr: 0,
            groups: [false; SPI_ID_MAX],
            interrupt: gic_int,
            line_asserted: [false; SPI_ID_MAX],
            routes: [0; SPI_ID_MAX],
        }
    }

    pub fn read(&self, offset: usize, width: usize) -> usize {
        let mut value = 0;
        for byte_index in 0..width {
            value |= (self.read_byte(offset + byte_index) as usize) << (byte_index * 8);
        }
        value
    }

    pub fn write(&mut self, offset: usize, width: usize, value: usize) {
        for byte_index in 0..width {
            self.write_byte(
                offset + byte_index,
                ((value >> (byte_index * 8)) & 0xff) as u8,
            );
        }
    }

    fn read_byte(&self, offset: usize) -> u8 {
        match offset {
            offset if (GICD_CTLR..GICD_CTLR + 4).contains(&offset) => {
                read_u32_byte(self.ctrlr, offset - GICD_CTLR)
            }
            offset if (GICD_TYPER..GICD_TYPER + 4).contains(&offset) => {
                read_u32_byte(GICD_TYPER_VALUE, offset - GICD_TYPER)
            }
            offset if (GICD_IIDR..GICD_IIDR + 4).contains(&offset) => {
                read_u32_byte(GICD_IIDR_VALUE, offset - GICD_IIDR)
            }
            offset if (GICD_TYPER2..GICD_TYPER2 + 4).contains(&offset) => 0,
            offset if (GICD_STATUSR..GICD_STATUSR + 4).contains(&offset) => 0,
            offset if GICD_IGROUPR.contains(&offset) => {
                self.read_interrupt_bits(offset - GICD_IGROUPR.start, |irq| self.groups[irq])
            }
            offset if GICD_ISENABLER.contains(&offset) => self
                .read_interrupt_bits(offset - GICD_ISENABLER.start, |irq| {
                    self.interrupt[irq].get_enable()
                }),
            offset if GICD_ICENABLER.contains(&offset) => self
                .read_interrupt_bits(offset - GICD_ICENABLER.start, |irq| {
                    self.interrupt[irq].get_enable()
                }),
            offset if GICD_ISPENDR.contains(&offset) => self
                .read_interrupt_bits(offset - GICD_ISPENDR.start, |irq| {
                    interrupt_is_pending(self.interrupt[irq].get_status())
                }),
            offset if GICD_ICPENDR.contains(&offset) => self
                .read_interrupt_bits(offset - GICD_ICPENDR.start, |irq| {
                    interrupt_is_pending(self.interrupt[irq].get_status())
                }),
            offset if GICD_ISACTIVER.contains(&offset) => self
                .read_interrupt_bits(offset - GICD_ISACTIVER.start, |irq| {
                    interrupt_is_active(self.interrupt[irq].get_status())
                }),
            offset if GICD_ICACTIVER.contains(&offset) => self
                .read_interrupt_bits(offset - GICD_ICACTIVER.start, |irq| {
                    interrupt_is_active(self.interrupt[irq].get_status())
                }),
            offset if GICD_IPRIORITYR.contains(&offset) => {
                let irq = offset - GICD_IPRIORITYR.start;
                if self.is_distributor_spi(irq) {
                    self.interrupt[irq].get_priority() as u8
                } else {
                    0
                }
            }
            offset if GICD_ITARGETSR.contains(&offset) => 0,
            offset if GICD_ICFGR.contains(&offset) => {
                self.read_configuration_byte(offset - GICD_ICFGR.start)
            }
            offset if GICD_IROUTER.contains(&offset) => {
                self.read_route_byte(offset - GICD_IROUTER.start)
            }
            offset if (GICD_PIDR2..GICD_PIDR2 + 4).contains(&offset) => {
                read_u32_byte(0x3b, offset - GICD_PIDR2)
            }
            _ => 0,
        }
    }

    fn write_byte(&mut self, offset: usize, value: u8) {
        match offset {
            offset if (GICD_CTLR..GICD_CTLR + 4).contains(&offset) => {
                let shift = (offset - GICD_CTLR) * 8;
                let mask = 0xff_u32 << shift;
                self.ctrlr = (self.ctrlr & !mask) | ((value as u32) << shift);
                self.ctrlr &= GICD_CTLR_WRITABLE_MASK;
            }
            offset if GICD_IGROUPR.contains(&offset) => {
                let byte_index = offset - GICD_IGROUPR.start;
                self.write_interrupt_bits(byte_index, value, |this, irq, set| {
                    this.groups[irq] = set;
                });
            }
            offset if GICD_ISENABLER.contains(&offset) => {
                let byte_index = offset - GICD_ISENABLER.start;
                self.write_set_bits(byte_index, value, |interrupt| {
                    interrupt.set_enable(true);
                });
            }
            offset if GICD_ICENABLER.contains(&offset) => {
                let byte_index = offset - GICD_ICENABLER.start;
                self.write_set_bits(byte_index, value, |interrupt| {
                    interrupt.set_enable(false);
                });
            }
            offset if GICD_ISPENDR.contains(&offset) => {
                let byte_index = offset - GICD_ISPENDR.start;
                self.write_set_bits(byte_index, value, set_pending);
            }
            offset if GICD_ICPENDR.contains(&offset) => {
                let byte_index = offset - GICD_ICPENDR.start;
                self.write_set_bits(byte_index, value, clear_pending);
            }
            offset if GICD_ISACTIVER.contains(&offset) => {
                let byte_index = offset - GICD_ISACTIVER.start;
                self.write_set_bits(byte_index, value, set_active);
            }
            offset if GICD_ICACTIVER.contains(&offset) => {
                let byte_index = offset - GICD_ICACTIVER.start;
                self.write_set_bits(byte_index, value, clear_active);
            }
            offset if GICD_IPRIORITYR.contains(&offset) => {
                let irq = offset - GICD_IPRIORITYR.start;
                if self.is_distributor_spi(irq) {
                    self.interrupt[irq].set_priority(value as u32);
                }
            }
            offset if GICD_ITARGETSR.contains(&offset) => {}
            offset if GICD_ICFGR.contains(&offset) => {
                self.write_configuration_byte(offset - GICD_ICFGR.start, value);
            }
            offset if GICD_IROUTER.contains(&offset) => {
                self.write_route_byte(offset - GICD_IROUTER.start, value);
            }
            _ => {}
        }
    }

    fn read_interrupt_bits(&self, byte_index: usize, predicate: impl Fn(usize) -> bool) -> u8 {
        let first_irq = byte_index * 8;
        let mut value = 0;
        for bit in 0..8 {
            let irq = first_irq + bit;
            if self.is_distributor_spi(irq) && predicate(irq) {
                value |= 1 << bit;
            }
        }
        value
    }

    fn write_interrupt_bits(
        &mut self,
        byte_index: usize,
        value: u8,
        mut update: impl FnMut(&mut Self, usize, bool),
    ) {
        let first_irq = byte_index * 8;
        for bit in 0..8 {
            let irq = first_irq + bit;
            if self.is_distributor_spi(irq) {
                update(self, irq, value & (1 << bit) != 0);
            }
        }
    }

    fn write_set_bits(
        &mut self,
        byte_index: usize,
        value: u8,
        mut update: impl FnMut(&mut VgicInt),
    ) {
        let first_irq = byte_index * 8;
        for bit in 0..8 {
            let irq = first_irq + bit;
            if self.is_distributor_spi(irq) && value & (1 << bit) != 0 {
                update(&mut self.interrupt[irq]);
            }
        }
    }

    fn read_configuration_byte(&self, byte_index: usize) -> u8 {
        let first_irq = byte_index * 4;
        let mut value = 0;
        for field in 0..4 {
            let irq = first_irq + field;
            if self.is_distributor_spi(irq)
                && matches!(self.interrupt[irq].get_trigger_mode(), TriggerMode::Edge)
            {
                value |= 0b10 << (field * 2);
            }
        }
        value
    }

    fn write_configuration_byte(&mut self, byte_index: usize, value: u8) {
        let first_irq = byte_index * 4;
        for field in 0..4 {
            let irq = first_irq + field;
            if self.is_distributor_spi(irq) {
                let trigger = if value & (0b10 << (field * 2)) != 0 {
                    TriggerMode::Edge
                } else {
                    TriggerMode::Level
                };
                self.interrupt[irq].set_trigger_mode(trigger);
            }
        }
    }

    fn read_route_byte(&self, byte_offset: usize) -> u8 {
        let irq = byte_offset / 8;
        let route_byte = byte_offset % 8;
        if self.is_distributor_spi(irq) {
            ((self.routes[irq] >> (route_byte * 8)) & 0xff) as u8
        } else {
            0
        }
    }

    fn write_route_byte(&mut self, byte_offset: usize, value: u8) {
        let irq = byte_offset / 8;
        let route_byte = byte_offset % 8;
        if self.is_distributor_spi(irq) {
            let shift = route_byte * 8;
            let mask = 0xff_u64 << shift;
            self.routes[irq] = (self.routes[irq] & !mask) | ((value as u64) << shift);
        }
    }

    fn is_distributor_spi(&self, irq: usize) -> bool {
        (PPI_ID_MAX..SPI_ID_MAX).contains(&irq)
    }

    pub fn fetch_irq(&self, idx: u32) -> VgicInt {
        let idx = idx as usize;
        let mut interrupt = self.interrupt[idx];
        interrupt.set_vcpu_id((self.routes[idx] & 0xff) as u32);
        interrupt
    }

    pub(crate) fn irq_enabled(&self, idx: u32) -> bool {
        self.interrupt
            .get(idx as usize)
            .is_some_and(VgicInt::get_enable)
    }

    pub(crate) fn irq_route(&self, idx: u32) -> crate::VgicResult<u64> {
        let idx = idx as usize;
        self.routes
            .get(idx)
            .copied()
            .ok_or(crate::VgicError::InvalidIrq {
                irq: idx,
                max: SPI_ID_MAX,
            })
    }

    pub(crate) fn set_irq_line_level(
        &mut self,
        idx: u32,
        asserted: bool,
    ) -> crate::VgicResult<bool> {
        let idx = idx as usize;
        let Some(line_asserted) = self.line_asserted.get_mut(idx) else {
            return Err(crate::VgicError::InvalidIrq {
                irq: idx,
                max: SPI_ID_MAX,
            });
        };
        let newly_asserted = asserted && !*line_asserted;
        *line_asserted = asserted;
        Ok(newly_asserted && self.interrupt[idx].get_enable())
    }

    pub(crate) fn asserted_enabled_irqs(&self) -> Vec<u32> {
        (PPI_ID_MAX..SPI_ID_MAX)
            .filter(|idx| self.line_asserted[*idx] && self.interrupt[*idx].get_enable())
            .map(|idx| idx as u32)
            .collect()
    }
}

fn read_u32_byte(value: u32, byte_index: usize) -> u8 {
    ((value >> (byte_index * 8)) & 0xff) as u8
}

fn interrupt_is_pending(status: InterruptStatus) -> bool {
    matches!(
        status,
        InterruptStatus::Pending | InterruptStatus::ActivePending
    )
}

fn interrupt_is_active(status: InterruptStatus) -> bool {
    matches!(
        status,
        InterruptStatus::Active | InterruptStatus::ActivePending
    )
}

fn set_pending(interrupt: &mut VgicInt) {
    let status = match interrupt.get_status() {
        InterruptStatus::Inactive => InterruptStatus::Pending,
        InterruptStatus::Active => InterruptStatus::ActivePending,
        status => status,
    };
    interrupt.set_status(status);
}

fn clear_pending(interrupt: &mut VgicInt) {
    let status = match interrupt.get_status() {
        InterruptStatus::Pending => InterruptStatus::Inactive,
        InterruptStatus::ActivePending => InterruptStatus::Active,
        status => status,
    };
    interrupt.set_status(status);
}

fn set_active(interrupt: &mut VgicInt) {
    let status = match interrupt.get_status() {
        InterruptStatus::Inactive => InterruptStatus::Active,
        InterruptStatus::Pending => InterruptStatus::ActivePending,
        status => status,
    };
    interrupt.set_status(status);
}

fn clear_active(interrupt: &mut VgicInt) {
    let status = match interrupt.get_status() {
        InterruptStatus::Active => InterruptStatus::Inactive,
        InterruptStatus::ActivePending => InterruptStatus::Pending,
        status => status,
    };
    interrupt.set_status(status);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typer_id_bits_cover_every_emulated_interrupt() {
        let typer = Vgicd::new().read(GICD_TYPER, core::mem::size_of::<u32>()) as u32;
        let id_bits = (typer >> 19) & 0x1f;
        let max_intid = 1u32 << (id_bits + 1);

        assert_eq!(max_intid, SPI_ID_MAX as u32);
        assert!(27 < max_intid, "virtual timer PPI must be configurable");
    }
}
