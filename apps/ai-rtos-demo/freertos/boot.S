/* Copyright 2026 The TGOSKits Authors */
/* SPDX-License-Identifier: Apache-2.0 */

    .section .text.boot, "ax"
    .globl _boot
    .type _boot, %function

_boot:
    mov x19, x0

    /*
     * AxVisor enters a guest at EL1.  Standalone QEMU with virtualization
     * enabled starts the payload at EL2 instead.  Normalize both standard
     * AArch64 entry contracts to EL1 before installing the FreeRTOS vectors.
     */
    mrs x0, CurrentEL
    cmp x0, #(2 << 2)
    b.ne 1f

    mov x0, #(1 << 31)             /* HCR_EL2.RW: EL1 is AArch64. */
    msr HCR_EL2, x0

    mrs x0, CNTHCTL_EL2
    orr x0, x0, #3                 /* Allow EL1 physical counter/timer access. */
    msr CNTHCTL_EL2, x0
    msr CNTVOFF_EL2, xzr
    msr CPTR_EL2, xzr

    mov x0, #0x3c5                 /* Mask DAIF and return to EL1h. */
    msr SPSR_EL2, x0
    adr x0, 1f
    msr ELR_EL2, x0
    isb
    eret

1:
    ldr x0, =_vector_table
    msr VBAR_EL1, x0

    mrs x0, CPACR_EL1
    orr x0, x0, #(3 << 20)
    msr CPACR_EL1, x0
    isb

    ldr x0, =__boot_stack_top
    mov sp, x0

    ldr x0, =__bss_start__
    ldr x1, =__bss_end__
    mov x2, #0
2:
    cmp x0, x1
    b.hs 3f
    str x2, [x0], #8
    b 2b
3:
    mov x0, x19
    bl aicp_platform_set_dtb
    bl main
4:
    wfi
    b 4b
