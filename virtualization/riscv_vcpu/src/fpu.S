// Copyright 2025 The Axvisor Team
//
// Licensed under the Apache License, Version 2.0 (the "License");

.macro SAVE_D_CONTEXT base
    csrr t2, sstatus
    li t1, 0x6000
    and t2, t2, t1
    csrs sstatus, t1
    .irp n,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
        fsd f\n, (\n * 8)(\base)
    .endr
    frcsr t0
    sw t0, 256(\base)
    csrc sstatus, t1
    csrs sstatus, t2
    ret
.endm

.macro LOAD_D_CONTEXT base
    csrr t2, sstatus
    li t1, 0x6000
    and t2, t2, t1
    csrs sstatus, t1
    .irp n,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
        fld f\n, (\n * 8)(\base)
    .endr
    lw t0, 256(\base)
    fscsr t0
    csrc sstatus, t1
    csrs sstatus, t2
    ret
.endm

.text
.option push
.option arch, +f
.option arch, +d

.global _axvisor_save_fpu_context_d
.type _axvisor_save_fpu_context_d, @function
_axvisor_save_fpu_context_d:
    SAVE_D_CONTEXT a0
.size _axvisor_save_fpu_context_d, .-_axvisor_save_fpu_context_d

.global _axvisor_load_fpu_context_d
.type _axvisor_load_fpu_context_d, @function
_axvisor_load_fpu_context_d:
    LOAD_D_CONTEXT a0
.size _axvisor_load_fpu_context_d, .-_axvisor_load_fpu_context_d

.option pop
