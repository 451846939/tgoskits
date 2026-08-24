/* Copyright 2026 The TGOSKits Authors */
/* SPDX-License-Identifier: Apache-2.0 */

#include "fdt_virtio.h"

#include <stddef.h>

#define FDT_MAGIC 0xd00dfeedU
#define FDT_HEADER_SIZE 40U
#define FDT_TOKEN_BEGIN_NODE 1U
#define FDT_TOKEN_END_NODE 2U
#define FDT_TOKEN_PROP 3U
#define FDT_TOKEN_NOP 4U
#define FDT_TOKEN_END 9U
#define FDT_MAX_DEPTH 16U

struct fdt_header {
    uint32_t magic;
    uint32_t total_size;
    uint32_t struct_offset;
    uint32_t strings_offset;
    uint32_t reserve_map_offset;
    uint32_t version;
    uint32_t compatible_version;
    uint32_t boot_cpu;
    uint32_t strings_size;
    uint32_t struct_size;
};

struct fdt_node {
    bool is_virtio_mmio;
    bool has_register;
    bool has_interrupt;
    uintptr_t base;
    uint32_t irq;
};

static uint32_t read_be32(const uint8_t *value)
{
    return ((uint32_t)value[0] << 24U) | ((uint32_t)value[1] << 16U) |
           ((uint32_t)value[2] << 8U) | (uint32_t)value[3];
}

static bool read_word(const uint8_t *cursor, const uint8_t *end, uint32_t *value)
{
    if ((size_t)(end - cursor) < sizeof(uint32_t)) {
        return false;
    }
    *value = read_be32(cursor);
    return true;
}

static const uint8_t *align_word(const uint8_t *cursor, const uint8_t *end)
{
    const uintptr_t aligned = ((uintptr_t)cursor + 3U) & ~(uintptr_t)3U;

    if (aligned < (uintptr_t)cursor || aligned > (uintptr_t)end) {
        return NULL;
    }
    return (const uint8_t *)aligned;
}

static bool contains_compatible(const uint8_t *value, uint32_t length, const char *expected)
{
    size_t offset = 0U;
    size_t expected_length = 0U;

    while (expected[expected_length] != '\0') {
        expected_length++;
    }
    while (offset < length) {
        size_t item_length = 0U;
        while (offset + item_length < length && value[offset + item_length] != '\0') {
            item_length++;
        }
        if (item_length == expected_length) {
            size_t index;
            for (index = 0U; index < expected_length; index++) {
                if (value[offset + index] != (uint8_t)expected[index]) {
                    break;
                }
            }
            if (index == expected_length) {
                return true;
            }
        }
        if (offset + item_length == length) {
            return false;
        }
        offset += item_length + 1U;
    }
    return false;
}

static bool property_name_equals(const uint8_t *strings,
                                 uint32_t strings_size,
                                 uint32_t offset,
                                 const char *expected)
{
    size_t index = 0U;

    if (offset >= strings_size) {
        return false;
    }
    while (offset + index < strings_size && expected[index] != '\0') {
        if (strings[offset + index] != (uint8_t)expected[index]) {
            return false;
        }
        index++;
    }
    return expected[index] == '\0' && offset + index < strings_size &&
           strings[offset + index] == '\0';
}

static bool parse_register(const uint8_t *value,
                           uint32_t length,
                           uint32_t address_cells,
                           uint32_t size_cells,
                           uintptr_t *base)
{
    uint32_t high = 0U;
    uint32_t low;
    const uint32_t cells = address_cells + size_cells;

    if ((address_cells != 1U && address_cells != 2U) || size_cells > 2U ||
        length < cells * sizeof(uint32_t)) {
        return false;
    }
    if (address_cells == 2U) {
        high = read_be32(value);
    }
    low = read_be32(value + (address_cells - 1U) * sizeof(uint32_t));
    if (high != 0U) {
        return false;
    }
    *base = (uintptr_t)low;
    return true;
}

static bool parse_gic_interrupt(const uint8_t *value, uint32_t length, uint32_t *irq)
{
    const uint32_t kind = read_be32(value);
    const uint32_t number = read_be32(value + sizeof(uint32_t));

    if (length < 3U * sizeof(uint32_t) || kind != 0U || number > UINT32_MAX - 32U) {
        return false;
    }
    *irq = number + 32U;
    return true;
}

bool aicp_fdt_find_virtio_mmio(const void *dtb,
                                struct aicp_virtio_mmio_resource *resource)
{
    const uint8_t *blob = (const uint8_t *)dtb;
    const struct fdt_header *header;
    const uint8_t *end;
    const uint8_t *strings;
    const uint8_t *cursor;
    const uint8_t *struct_end;
    struct fdt_node nodes[FDT_MAX_DEPTH] = {0};
    uint32_t address_cells = 2U;
    uint32_t size_cells = 2U;
    uint32_t depth = 0U;

    if (blob == NULL || resource == NULL) {
        return false;
    }
    header = (const struct fdt_header *)blob;
    if (read_be32((const uint8_t *)&header->magic) != FDT_MAGIC ||
        read_be32((const uint8_t *)&header->total_size) < FDT_HEADER_SIZE) {
        return false;
    }
    end = blob + read_be32((const uint8_t *)&header->total_size);
    cursor = blob + read_be32((const uint8_t *)&header->struct_offset);
    strings = blob + read_be32((const uint8_t *)&header->strings_offset);
    struct_end = cursor + read_be32((const uint8_t *)&header->struct_size);
    if (cursor < blob || strings < blob || cursor > end || strings > end ||
        struct_end < cursor || struct_end > end ||
        (size_t)(end - strings) < read_be32((const uint8_t *)&header->strings_size)) {
        return false;
    }

    while (cursor < struct_end) {
        uint32_t token;

        if (!read_word(cursor, struct_end, &token)) {
            return false;
        }
        cursor += sizeof(uint32_t);
        if (token == FDT_TOKEN_BEGIN_NODE) {
            if (depth == FDT_MAX_DEPTH) {
                return false;
            }
            while (cursor < struct_end && *cursor != '\0') {
                cursor++;
            }
            if (cursor == struct_end) {
                return false;
            }
            cursor = align_word(cursor + 1U, struct_end);
            if (cursor == NULL) {
                return false;
            }
            nodes[depth++] = (struct fdt_node){0};
        } else if (token == FDT_TOKEN_END_NODE) {
            struct fdt_node *node;

            if (depth == 0U) {
                return false;
            }
            node = &nodes[--depth];
            if (node->is_virtio_mmio && node->has_register && node->has_interrupt) {
                resource->base = node->base;
                resource->irq = node->irq;
                return true;
            }
        } else if (token == FDT_TOKEN_PROP) {
            uint32_t length;
            uint32_t name_offset;
            const uint8_t *value;
            struct fdt_node *node;

            if (depth == 0U || !read_word(cursor, struct_end, &length) ||
                !read_word(cursor + sizeof(uint32_t), struct_end, &name_offset)) {
                return false;
            }
            cursor += 2U * sizeof(uint32_t);
            value = cursor;
            if ((size_t)(struct_end - value) < length) {
                return false;
            }
            cursor = align_word(value + length, struct_end);
            if (cursor == NULL) {
                return false;
            }
            node = &nodes[depth - 1U];
            if (depth == 1U && property_name_equals(strings,
                                                    read_be32((const uint8_t *)&header->strings_size),
                                                    name_offset,
                                                    "#address-cells") &&
                length == sizeof(uint32_t)) {
                address_cells = read_be32(value);
            } else if (depth == 1U && property_name_equals(strings,
                                                           read_be32((const uint8_t *)&header->strings_size),
                                                           name_offset,
                                                           "#size-cells") &&
                       length == sizeof(uint32_t)) {
                size_cells = read_be32(value);
            } else if (property_name_equals(strings,
                                             read_be32((const uint8_t *)&header->strings_size),
                                             name_offset,
                                             "compatible")) {
                node->is_virtio_mmio = contains_compatible(value, length, "virtio,mmio");
            } else if (property_name_equals(strings,
                                             read_be32((const uint8_t *)&header->strings_size),
                                             name_offset,
                                             "reg")) {
                node->has_register = parse_register(value, length, address_cells, size_cells,
                                                    &node->base);
            } else if (property_name_equals(strings,
                                             read_be32((const uint8_t *)&header->strings_size),
                                             name_offset,
                                             "interrupts")) {
                node->has_interrupt = parse_gic_interrupt(value, length, &node->irq);
            }
        } else if (token == FDT_TOKEN_NOP) {
            continue;
        } else if (token == FDT_TOKEN_END) {
            return false;
        } else {
            return false;
        }
    }
    return false;
}
