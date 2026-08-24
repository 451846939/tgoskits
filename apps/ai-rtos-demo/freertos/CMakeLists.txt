# Copyright 2026 The TGOSKits Authors
# SPDX-License-Identifier: Apache-2.0

cmake_minimum_required(VERSION 3.20)
project(aicp_freertos_guest C ASM)

option(AICP_FREERTOS_BASELINE "Build the native periodic-latency baseline" OFF)
option(AICP_FREERTOS_STRESS "Add CPU stress workers to the periodic baseline" OFF)

if(NOT DEFINED FREERTOS_KERNEL_DIR)
  message(FATAL_ERROR "FREERTOS_KERNEL_DIR is required")
endif()
if(NOT DEFINED FREERTOS_PLUS_TCP_DIR)
  message(FATAL_ERROR "FREERTOS_PLUS_TCP_DIR is required")
endif()

file(GLOB FREERTOS_PLUS_TCP_SOURCES CONFIGURE_DEPENDS
  ${FREERTOS_PLUS_TCP_DIR}/source/*.c
)

set(CMAKE_C_STANDARD 11)
set(CMAKE_C_STANDARD_REQUIRED ON)

add_executable(aicp-freertos.elf
  boot.S
  vectors.S
  libc.c
  fdt_virtio.c
  platform.c
  virtio_net.c
  NetworkInterface.c
  main.c
  ../rtos-core/aicp_service.c
  ../rtos-core/control_loop.c
  ${FREERTOS_KERNEL_DIR}/list.c
  ${FREERTOS_KERNEL_DIR}/queue.c
  ${FREERTOS_KERNEL_DIR}/tasks.c
  ${FREERTOS_KERNEL_DIR}/event_groups.c
  ${FREERTOS_KERNEL_DIR}/portable/MemMang/heap_4.c
  ${FREERTOS_KERNEL_DIR}/portable/GCC/ARM_AARCH64_SRE/port.c
  ${FREERTOS_KERNEL_DIR}/portable/GCC/ARM_AARCH64_SRE/portASM.S
  ${FREERTOS_PLUS_TCP_SOURCES}
  ${FREERTOS_PLUS_TCP_DIR}/source/portable/BufferManagement/BufferAllocation_2.c
)

target_include_directories(aicp-freertos.elf PRIVATE
  ${CMAKE_CURRENT_SOURCE_DIR}
  ${CMAKE_CURRENT_SOURCE_DIR}/../aicp
  ${CMAKE_CURRENT_SOURCE_DIR}/../rtos-core
  ${FREERTOS_KERNEL_DIR}/include
  ${FREERTOS_KERNEL_DIR}/portable/GCC/ARM_AARCH64_SRE
  ${FREERTOS_PLUS_TCP_DIR}/source/include
  ${FREERTOS_PLUS_TCP_DIR}/source/portable/Compiler/GCC
)

target_compile_definitions(
    aicp-freertos.elf
    PRIVATE
        GUEST=1
        AICP_FREERTOS=1
        QEMU=1
)
if(AICP_FREERTOS_BASELINE)
  target_compile_definitions(aicp-freertos.elf PRIVATE AICP_FREERTOS_BASELINE=1)
endif()
if(AICP_FREERTOS_STRESS)
  target_compile_definitions(aicp-freertos.elf PRIVATE AICP_FREERTOS_STRESS=1)
endif()
target_compile_options(aicp-freertos.elf PRIVATE
  -ffreestanding -fno-common -fno-pic -fno-pie
  -mcmodel=large -march=armv8-a
  -Wall -Wextra -Werror
  $<$<COMPILE_LANGUAGE:C>:-include;string.h>
  $<$<COMPILE_LANGUAGE:C>:-fno-builtin>
)
target_link_options(aicp-freertos.elf PRIVATE
  -nostdlib -nostartfiles -static -no-pie
  -Wl,--build-id=none
  -Wl,-T${CMAKE_CURRENT_SOURCE_DIR}/linker.ld
)
target_link_libraries(aicp-freertos.elf PRIVATE gcc)

add_custom_command(TARGET aicp-freertos.elf POST_BUILD
  COMMAND ${CMAKE_OBJCOPY} -O binary
          $<TARGET_FILE:aicp-freertos.elf>
          ${CMAKE_CURRENT_BINARY_DIR}/aicp-freertos.bin
  BYPRODUCTS ${CMAKE_CURRENT_BINARY_DIR}/aicp-freertos.bin
)
