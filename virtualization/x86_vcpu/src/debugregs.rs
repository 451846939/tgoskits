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

use core::arch::asm;

const DEBUG_CONTROL_DISABLED: u64 = 0x400;

/// Debug-address registers shared by the host and guest on x86.
#[derive(Debug)]
pub(crate) struct DebugRegisterState {
    pub(crate) guest_db: [u64; 4],
    host_db: [u64; 4],
    host_dr7: u64,
}

impl DebugRegisterState {
    pub(crate) const fn new() -> Self {
        Self {
            guest_db: [0; 4],
            host_db: [0; 4],
            host_dr7: DEBUG_CONTROL_DISABLED,
        }
    }

    /// Saves host debug addresses and installs the guest addresses.
    pub(crate) fn switch_to_guest(&mut self) {
        self.host_dr7 = read_dr7();
        write_dr7(DEBUG_CONTROL_DISABLED);
        self.host_db = read_db();
        write_db(self.guest_db);
    }

    /// Captures guest debug addresses and restores the host state.
    pub(crate) fn switch_to_host(&mut self) {
        write_dr7(DEBUG_CONTROL_DISABLED);
        self.guest_db = read_db();
        write_db(self.host_db);
        write_dr7(self.host_dr7);
    }
}

#[cfg(feature = "vmx")]
pub(crate) fn read_dr6() -> u64 {
    let value: u64;
    unsafe { asm!("mov {}, dr6", out(reg) value, options(nomem, nostack, preserves_flags)) };
    value
}

#[cfg(feature = "vmx")]
pub(crate) fn write_dr6(value: u64) {
    unsafe { asm!("mov dr6, {}", in(reg) value, options(nomem, nostack, preserves_flags)) };
}

fn read_dr7() -> u64 {
    let value: u64;
    unsafe { asm!("mov {}, dr7", out(reg) value, options(nomem, nostack, preserves_flags)) };
    value
}

fn write_dr7(value: u64) {
    unsafe { asm!("mov dr7, {}", in(reg) value, options(nomem, nostack, preserves_flags)) };
}

fn read_db() -> [u64; 4] {
    let (dr0, dr1, dr2, dr3): (u64, u64, u64, u64);
    unsafe {
        asm!(
            "mov {dr0}, dr0",
            "mov {dr1}, dr1",
            "mov {dr2}, dr2",
            "mov {dr3}, dr3",
            dr0 = out(reg) dr0,
            dr1 = out(reg) dr1,
            dr2 = out(reg) dr2,
            dr3 = out(reg) dr3,
            options(nomem, nostack, preserves_flags),
        )
    };
    [dr0, dr1, dr2, dr3]
}

fn write_db(db: [u64; 4]) {
    unsafe {
        asm!(
            "mov dr0, {dr0}",
            "mov dr1, {dr1}",
            "mov dr2, {dr2}",
            "mov dr3, {dr3}",
            dr0 = in(reg) db[0],
            dr1 = in(reg) db[1],
            dr2 = in(reg) db[2],
            dr3 = in(reg) db[3],
            options(nomem, nostack, preserves_flags),
        )
    };
}
