/* Copyright 2026 The TGOSKits Authors */
/* SPDX-License-Identifier: Apache-2.0 */

ENTRY(_boot)

MEMORY
{
  ram (rwx) : ORIGIN = 0xD0000000, LENGTH = 0x08000000
}

SECTIONS
{
  . = ORIGIN(ram);

  .vectors : ALIGN(2048) {
    KEEP(*(.vectors))
  } > ram

  .text : ALIGN(16) {
    *(.text.boot)
    *(.text)
    *(.text.*)
    *(.rodata)
    *(.rodata.*)
  } > ram

  .data : ALIGN(16) {
    __data_start__ = .;
    *(.data)
    *(.data.*)
    __data_end__ = .;
  } > ram

  .bss (NOLOAD) : ALIGN(16) {
    __bss_start__ = .;
    *(.bss)
    *(.bss.*)
    *(COMMON)
    __bss_end__ = .;
  } > ram

  .boot_stack (NOLOAD) : ALIGN(16) {
    __boot_stack_bottom = .;
    . = . + 0x8000;
    __boot_stack_top = .;
  } > ram

  end = .;
  _end = .;
  __end__ = .;

  /DISCARD/ : {
    *(.comment)
    *(.note*)
    *(.eh_frame*)
  }
}
