cmake_minimum_required(VERSION 3.20.0)

find_package(Zephyr REQUIRED HINTS $ENV{ZEPHYR_BASE})
project(tgoskits_aicp_zephyr)

target_sources(app PRIVATE
  src/main.c
  ../rtos-core/aicp_service.c
  ../rtos-core/control_loop.c
)

option(AICP_ZEPHYR_VIRTIO_LEGACY
  "Use the application-owned legacy virtio-mmio transport for AxVisor"
  OFF
)

if(AICP_ZEPHYR_VIRTIO_LEGACY)
  target_sources(app PRIVATE src/virtio_mmio_legacy.c)
endif()

target_include_directories(app PRIVATE
  ../aicp
  ../rtos-core
)
