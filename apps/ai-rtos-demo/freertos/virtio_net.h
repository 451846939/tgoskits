/* Copyright 2026 The TGOSKits Authors */
/* SPDX-License-Identifier: Apache-2.0 */

#ifndef AICP_FREERTOS_VIRTIO_NET_H
#define AICP_FREERTOS_VIRTIO_NET_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

bool aicp_virtio_net_init( void );
bool aicp_virtio_net_send( const uint8_t * frame, size_t length );
bool aicp_virtio_net_receive( uint8_t * frame, size_t capacity, size_t * length );
void aicp_virtio_net_set_rx_task( void * task );
void aicp_virtio_net_isr( void );

#endif
