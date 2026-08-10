/* Copyright 2026 The TGOSKits Authors */
/* SPDX-License-Identifier: Apache-2.0 */

#ifndef AICP_FREERTOS_PLATFORM_H
#define AICP_FREERTOS_PLATFORM_H

#include <stddef.h>
#include <stdint.h>

void aicp_platform_set_dtb( const void * dtb );
void aicp_platform_init( void );
void aicp_uart_putc( char value );
void aicp_uart_puts( const char * value );
void aicp_uart_printf( const char * format, ... );
uint64_t aicp_counter_read( void );
uint64_t aicp_counter_frequency( void );
void aicp_delay_us( uint32_t usec );
void aicp_platform_enable_net_irq( void );
void aicp_virtio_net_isr( void );

void vConfigureTickInterrupt( void );
void vClearTickInterrupt( void );
void vApplicationIRQHandler( uint32_t iar );

#endif
