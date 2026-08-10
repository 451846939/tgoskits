/* Copyright 2026 The TGOSKits Authors */
/* SPDX-License-Identifier: Apache-2.0 */

    .section .vectors, "ax"
    .globl _vector_table
    .globl _freertos_vector_table

    .macro VECTOR target
    b \target
    .space 0x7c
    .endm

_vector_table:
    VECTOR _boot
    VECTOR .
    VECTOR .
    VECTOR .
    VECTOR .
    VECTOR .
    VECTOR .
    VECTOR .
    VECTOR .
    VECTOR .
    VECTOR .
    VECTOR .
    VECTOR .
    VECTOR .
    VECTOR .
    VECTOR .

    .align 11
_freertos_vector_table:
    VECTOR FreeRTOS_SWI_Handler
    VECTOR FreeRTOS_IRQ_Handler
    VECTOR .
    VECTOR .
    VECTOR FreeRTOS_SWI_Handler
    VECTOR FreeRTOS_IRQ_Handler
    VECTOR .
    VECTOR .
    VECTOR .
    VECTOR .
    VECTOR .
    VECTOR .
    VECTOR .
    VECTOR .
    VECTOR .
    VECTOR .
