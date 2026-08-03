// Copyright 2026 The Axvisor Team
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

fn ordered(source: &str, needles: &[&str]) {
    let mut cursor = 0;
    for needle in needles {
        let relative = source[cursor..]
            .find(needle)
            .unwrap_or_else(|| panic!("missing timer world-switch operation: {needle}"));
        cursor += relative + needle.len();
    }
}

#[test]
fn exception_vector_table_uses_sixteen_fixed_width_branch_slots() {
    let assembly = include_str!("../src/exception.S");
    let vector_start = assembly.find("exception_vector_base_vcpu:").unwrap();
    let vector_end = assembly[vector_start..]
        .find(".global context_vm_entry")
        .unwrap()
        + vector_start;
    let vector_table = &assembly[vector_start..vector_end];

    assert_eq!(
        vector_table.matches("VECTOR_SLOT ").count(),
        16,
        "AArch64 VBAR_EL2 requires sixteen 0x80-byte vector slots"
    );
    assert!(
        !vector_table.contains("SAVE_REGS_FROM_EL1")
            && !vector_table.contains("SAVE_VCPU_REGS_FROM_EL1"),
        "full exception handlers must stay outside the fixed-width vector table"
    );

    let slot_macro_start = assembly.find(".macro VECTOR_SLOT").unwrap();
    let slot_macro_end = assembly[slot_macro_start..].find(".endm").unwrap() + slot_macro_start;
    let slot_macro = &assembly[slot_macro_start..slot_macro_end];
    ordered(slot_macro, &["b       \\handler", ".space  0x80 - 4"]);
}

#[test]
fn guest_exit_stops_cntv_before_restoring_the_host_counter_domain() {
    let assembly = include_str!("../src/exception.S");
    let start = assembly.find(".macro SAVE_VCPU_RUNTIME_FROM_EL1").unwrap();
    let end = assembly[start..].find(".endm").unwrap() + start;
    let save = &assembly[start..end];

    ordered(
        save,
        &[
            "mrs     x9, cntv_ctl_el0",
            "mrs     x9, cntv_cval_el0",
            "mrs     x9, cntkctl_el1",
            "msr     cntv_ctl_el0, xzr",
            "isb",
            "msr     cntvoff_el2, xzr",
            "msr     cnthctl_el2, x9",
            "msr     cntkctl_el1, x9",
            "strb    wzr, [sp, {timer_loaded_offset}]",
            "isb",
        ],
    );
}

#[test]
fn lower_irq_acknowledges_host_source_before_stopping_cntv() {
    let assembly = include_str!("../src/exception.S");
    let start = assembly.find(".macro HANDLE_LOWER_IRQ_VCPU").unwrap();
    let end = assembly[start..].find(".endm").unwrap() + start;
    let handler = &assembly[start..end];

    ordered(
        handler,
        &[
            "SAVE_REGS_FROM_EL1",
            "ACK_PENDING_HOST_IRQ",
            "SAVE_VCPU_RUNTIME_FROM_EL1",
            "bl      vmexit_trampoline",
        ],
    );
}

#[test]
fn host_irq_acknowledgement_supports_gicv2_mmio_and_gicv3_sysreg() {
    let assembly = include_str!("../src/exception.S");
    let start = assembly.find(".macro ACK_PENDING_HOST_IRQ").unwrap();
    let end = assembly[start..].find(".endm").unwrap() + start;
    let acknowledge = &assembly[start..end];

    ordered(
        acknowledge,
        &[
            "mov     w9, #-1",
            "str     w9, [sp, {host_pending_irq_ack_offset}]",
            "ldr     x9, [sp, {host_irq_interface_offset}]",
            "mrs     x9, ICC_IAR1_EL1",
            "str     w9, [sp, {host_pending_irq_ack_offset}]",
            "dsb     sy",
        ],
    );
    ordered(
        acknowledge,
        &[
            ".Lack_host_irq_gicv2\\@:",
            "ldr     x10, [sp, {host_irq_cpu_interface_base_offset}]",
            "ldr     w9, [x10, #0xc]",
            "str     w9, [sp, {host_pending_irq_ack_offset}]",
            "dsb     sy",
        ],
    );
}

#[test]
fn guest_entry_installs_offset_and_compare_before_enabling_cntv() {
    let assembly = include_str!("../src/exception.S");
    let start = assembly.find(".Lexception_return_guest_el1:").unwrap();
    let end = assembly[start..]
        .find(".Lexception_return_current_el2:")
        .unwrap()
        + start;
    let load = &assembly[start..end];

    ordered(
        load,
        &[
            "mrs     x9, cnthctl_el2",
            "mrs     x9, cntkctl_el1",
            "msr     cntv_ctl_el0, xzr",
            "isb",
            "msr     cntvoff_el2, x9",
            "msr     cnthctl_el2, x9",
            "msr     cntkctl_el1, x9",
            "msr     cntv_cval_el0, x9",
            "isb",
            "msr     cntv_ctl_el0, x9",
            "strb    w9, [sp, {timer_loaded_offset}]",
            "isb",
            "eret",
        ],
    );
}
