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

#[cfg(not(test))]
use ax_kspin::SpinNoIrq as Mutex;
use axdevice_base::AccessWidth;
#[cfg(test)]
use spin::Mutex;

use crate::{VgicError, VgicResult, interrupt::VgicInt, registers::GICD_SIZE, vgicd::Vgicd};

/// Virtual Generic Interrupt Controller.
///
/// Manages virtual interrupt distribution for guest VMs.
pub struct Vgic {
    vgicd: Mutex<Vgicd>,
}

impl Default for Vgic {
    fn default() -> Self {
        Self::new()
    }
}

impl Vgic {
    /// Creates a new VGIC instance.
    pub fn new() -> Vgic {
        Vgic {
            vgicd: Mutex::new(Vgicd::new()),
        }
    }

    pub(crate) fn handle_read(&self, offset: usize, width: AccessWidth) -> VgicResult<usize> {
        validate_access("read", offset, width)?;
        Ok(self.vgicd.lock().read(offset, usize::from(width)))
    }

    pub(crate) fn handle_write(
        &self,
        offset: usize,
        width: AccessWidth,
        value: usize,
    ) -> VgicResult {
        validate_access("write", offset, width)?;
        self.vgicd.lock().write(offset, usize::from(width), value);
        Ok(())
    }

    /// Handles 32-bit read access to VGIC registers.
    pub fn handle_read32(&self, addr: usize) -> VgicResult<usize> {
        self.handle_read(addr, AccessWidth::Dword)
    }

    /// Handles 8-bit write access to VGIC registers.
    pub fn handle_write8(&self, addr: usize, value: usize) {
        let _ = self.handle_write(addr, AccessWidth::Byte, value);
    }

    /// Handles 16-bit write access to VGIC registers.
    pub fn handle_write16(&self, addr: usize, value: usize) {
        let _ = self.handle_write(addr, AccessWidth::Word, value);
    }

    /// Handles 32-bit write access to VGIC registers.
    pub fn handle_write32(&self, addr: usize, value: usize) {
        let _ = self.handle_write(addr, AccessWidth::Dword, value);
    }

    // Removed, interrupt injection in arm_vcpu
    // pub fn inject_irq(&self, irq: u32) {
    //     self.vgicd.lock().inject_irq(irq);
    // }

    /// Fetches interrupt information for the given IRQ number.
    pub fn fetch_irq(&self, irq: u32) -> VgicInt {
        self.vgicd.lock().fetch_irq(irq)
    }

    /// Placeholder method for unused operations.
    pub fn nothing(&self, _value: u32) {}
}

fn validate_access(operation: &'static str, offset: usize, width: AccessWidth) -> VgicResult {
    let Some(end) = offset.checked_add(usize::from(width)) else {
        return Err(VgicError::InvalidAccess {
            operation,
            offset,
            width,
        });
    };
    if end > GICD_SIZE {
        return Err(VgicError::InvalidAccess {
            operation,
            offset,
            width,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::Vgic;

    #[test]
    fn distributor_identifies_as_gicv3() {
        let vgic = Vgic::new();

        assert_eq!(vgic.handle_read32(0xffe8).unwrap() & 0xf0, 0x30);
    }
}
