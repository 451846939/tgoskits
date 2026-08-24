/* Copyright 2026 The TGOSKits Authors */
/* SPDX-License-Identifier: Apache-2.0 */

#include "fdt_virtio.h"

#include <assert.h>
#include <stdint.h>
#include <stdio.h>

/* A minimal FDT with a GIC SPI and a VirtIO-MMIO v2 transport. */
static const uint32_t test_dtb_words[] = {
    0xd00dfeed, 0x000000fc, 0x00000028, 0x000000c4, 0x00000028,
    0x00000011, 0x00000010, 0x00000000, 0x00000038, 0x0000009c,
    0x00000001, 0x00000000,
    0x00000003, 0x00000004, 0x00000000, 0x00000002,
    0x00000003, 0x00000004, 0x0000000f, 0x00000002,
    0x00000001, 0x76697274, 0x696f5f6d, 0x6d696f40, 0x30623030,
    0x30303030, 0x00000000,
    0x00000003, 0x0000000c, 0x0000001b, 0x76697274, 0x696f2c6d,
    0x6d696f00,
    0x00000003, 0x00000010, 0x00000026, 0x00000000, 0x0b000000,
    0x00000000, 0x00000200,
    0x00000003, 0x0000000c, 0x0000002a, 0x00000000, 0x00000030,
    0x00000001,
    0x00000002, 0x00000002, 0x00000009,
    0x23616464, 0x72657373, 0x2d63656c, 0x6c730023, 0x73697a65,
    0x2d63656c, 0x6c730063, 0x6f6d7061, 0x7469626c, 0x65007265,
    0x6700696e, 0x74657272, 0x75707473, 0x00,
};

static void write_be32(uint8_t *out, uint32_t value)
{
    out[0] = (uint8_t)(value >> 24U);
    out[1] = (uint8_t)(value >> 16U);
    out[2] = (uint8_t)(value >> 8U);
    out[3] = (uint8_t)value;
}

int main(void)
{
    uint8_t test_dtb[sizeof(test_dtb_words)];
    struct aicp_virtio_mmio_resource resource = {0};
    size_t index;

    for (index = 0U; index < sizeof(test_dtb_words) / sizeof(test_dtb_words[0]); index++) {
        write_be32(test_dtb + index * sizeof(uint32_t), test_dtb_words[index]);
    }

    assert(aicp_fdt_find_virtio_mmio(test_dtb, &resource));
    assert(resource.base == 0x0b000000UL);
    assert(resource.irq == 80U);
    puts("AICP_FREERTOS_FDT_VIRTIO_TEST_PASSED");
    return 0;
}
