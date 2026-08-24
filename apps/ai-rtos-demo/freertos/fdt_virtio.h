/* Copyright 2026 The TGOSKits Authors */
/* SPDX-License-Identifier: Apache-2.0 */

#ifndef AICP_FREERTOS_FDT_VIRTIO_H
#define AICP_FREERTOS_FDT_VIRTIO_H

#include <stdbool.h>
#include <stdint.h>

struct aicp_virtio_mmio_resource {
    uintptr_t base;
    uint32_t irq;
};

bool aicp_fdt_find_virtio_mmio(const void *dtb,
                                struct aicp_virtio_mmio_resource *resource);

#endif
