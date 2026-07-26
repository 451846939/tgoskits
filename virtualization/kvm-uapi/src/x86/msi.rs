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

//! Decoding helpers for the x86 `KVM_SIGNAL_MSI` payload.

use crate::{KvmMsi, KvmUapiError, Result};

const MSI_ADDRESS_BASE: u32 = 0xfee0_0000;
const MSI_ADDRESS_BASE_MASK: u32 = 0xfff0_0000;
const MSI_ADDRESS_DESTINATION_SHIFT: u32 = 12;
const MSI_ADDRESS_DESTINATION_MASK: u32 = 0xff;
const MSI_ADDRESS_DESTINATION_MODE_LOGICAL: u32 = 1 << 2;
const MSI_ADDRESS_RESERVED_MASK: u32 = 0x0000_0ff3;
const MSI_DATA_VECTOR_MASK: u32 = 0xff;
const MSI_DATA_DELIVERY_MODE_SHIFT: u32 = 8;
const MSI_DATA_DELIVERY_MODE_MASK: u32 = 0x7;
const MSI_DATA_ALLOWED_MASK: u32 = 0x7ff;
const MSI_DELIVERY_FIXED: u32 = 0;
const MSI_DELIVERY_LOWEST_PRIORITY: u32 = 1;

/// An x86 MSI after decoding its APIC destination and delivery mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct X86MsiRoute {
    /// Guest interrupt vector carried by the MSI data word.
    pub vector: u8,
    /// VM-local vCPU mask selected by the MSI address and delivery mode.
    pub target_mask: u64,
}

/// Decode an xAPIC MSI using the flat logical-destination model.
///
/// The current AxVisor x86 topology maps physical APIC IDs directly to VM-local
/// vCPU IDs. Logical destinations are interpreted as a flat vCPU bit mask,
/// matching the minimal virtual IOAPIC model. Extended x2APIC destinations,
/// device-ID-qualified MSI injection, and non-maskable delivery modes are not
/// advertised by this helper.
pub fn decode_msi_route(msi: KvmMsi, active_vcpu_mask: u64) -> Result<X86MsiRoute> {
    if msi.flags != 0
        || msi.address_hi != 0
        || msi.address_lo & MSI_ADDRESS_BASE_MASK != MSI_ADDRESS_BASE
        || msi.address_lo & MSI_ADDRESS_RESERVED_MASK != 0
        || msi.data & !MSI_DATA_ALLOWED_MASK != 0
    {
        return Err(KvmUapiError::Unsupported);
    }

    let vector = (msi.data & MSI_DATA_VECTOR_MASK) as u8;
    if vector < 16 {
        return Err(KvmUapiError::Unsupported);
    }

    let destination =
        (msi.address_lo >> MSI_ADDRESS_DESTINATION_SHIFT) & MSI_ADDRESS_DESTINATION_MASK;
    let logical = msi.address_lo & MSI_ADDRESS_DESTINATION_MODE_LOGICAL != 0;
    let mut target_mask = if destination == MSI_ADDRESS_DESTINATION_MASK {
        active_vcpu_mask
    } else if logical {
        active_vcpu_mask & u64::from(destination)
    } else if destination < u64::BITS {
        active_vcpu_mask & (1u64 << destination)
    } else {
        0
    };

    match (msi.data >> MSI_DATA_DELIVERY_MODE_SHIFT) & MSI_DATA_DELIVERY_MODE_MASK {
        MSI_DELIVERY_FIXED => {}
        MSI_DELIVERY_LOWEST_PRIORITY => {
            if target_mask != 0 {
                target_mask &= 1u64 << target_mask.trailing_zeros();
            }
        }
        _ => return Err(KvmUapiError::Unsupported),
    }

    Ok(X86MsiRoute {
        vector,
        target_mask,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msi(address_lo: u32, data: u32) -> KvmMsi {
        KvmMsi {
            address_lo,
            address_hi: 0,
            data,
            flags: 0,
            devid: 0,
        }
    }

    #[test]
    fn decodes_physical_and_broadcast_destinations() {
        assert_eq!(
            decode_msi_route(msi(MSI_ADDRESS_BASE | (2 << 12), 0x41), 0b1111),
            Ok(X86MsiRoute {
                vector: 0x41,
                target_mask: 0b0100,
            })
        );
        assert_eq!(
            decode_msi_route(msi(MSI_ADDRESS_BASE | (0xff << 12), 0x42), 0b1011),
            Ok(X86MsiRoute {
                vector: 0x42,
                target_mask: 0b1011,
            })
        );
    }

    #[test]
    fn decodes_flat_logical_and_lowest_priority_destinations() {
        let address = MSI_ADDRESS_BASE | (0b1010 << 12) | MSI_ADDRESS_DESTINATION_MODE_LOGICAL;
        assert_eq!(
            decode_msi_route(msi(address, 0x51), 0b1111),
            Ok(X86MsiRoute {
                vector: 0x51,
                target_mask: 0b1010,
            })
        );
        assert_eq!(
            decode_msi_route(
                msi(address, 0x52 | (MSI_DELIVERY_LOWEST_PRIORITY << 8)),
                0b1111
            ),
            Ok(X86MsiRoute {
                vector: 0x52,
                target_mask: 0b0010,
            })
        );
    }

    #[test]
    fn rejects_unsupported_msi_encodings() {
        assert_eq!(
            decode_msi_route(msi(0xfed0_0000, 0x40), 1),
            Err(KvmUapiError::Unsupported)
        );
        assert_eq!(
            decode_msi_route(msi(MSI_ADDRESS_BASE, 0x40 | (2 << 8)), 1),
            Err(KvmUapiError::Unsupported)
        );
        assert_eq!(
            decode_msi_route(msi(MSI_ADDRESS_BASE, 0x0f), 1),
            Err(KvmUapiError::Unsupported)
        );
    }
}
