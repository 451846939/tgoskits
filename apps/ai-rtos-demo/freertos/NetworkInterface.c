/* Copyright 2026 The TGOSKits Authors */
/* SPDX-License-Identifier: Apache-2.0 */

#include "FreeRTOS.h"
#include "FreeRTOS_IP.h"
#include "FreeRTOS_IP_Private.h"
#include "NetworkBufferManagement.h"
#include "NetworkInterface.h"
#include "task.h"

#include "platform.h"
#include "virtio_net.h"

#include <string.h>

#define AICP_RX_TASK_STACK_WORDS  2048U
#define AICP_RX_TASK_PRIORITY     ( configMAX_PRIORITIES - 2U )
#define AICP_RX_FRAME_SIZE        1536U
#define AICP_NET_TRACE_LIMIT      16U
#define AICP_RX_RECOVERY_MS       20U

static NetworkInterface_t * network_interface;
static TaskHandle_t rx_task;
static uint8_t rx_frame[AICP_RX_FRAME_SIZE];
static uint32_t poll_recovery_count;
static uint32_t rx_trace_count;
static uint32_t tx_trace_count;

static uint16_t read_be16( const uint8_t * data )
{
    return ( uint16_t ) ( ( ( uint16_t ) data[0] << 8U ) | data[1] );
}

static void trace_packet( const char * direction,
                          const uint8_t * frame,
                          size_t length,
                          uint32_t * count )
{
    if( *count >= AICP_NET_TRACE_LIMIT ) {
        return;
    }
    ( *count )++;

    if( length < 14U ) {
        aicp_uart_printf( "AICP_FREERTOS_NET_PACKET dir=%s count=%u len=%u kind=short\n",
                          direction,
                          *count,
                          ( unsigned int ) length );
        return;
    }

    size_t network_offset = 14U;
    uint16_t ether_type = read_be16( frame + 12U );
    if( ether_type == 0x8100U && length >= 18U ) {
        ether_type = read_be16( frame + 16U );
        network_offset = 18U;
    }

    if( ether_type == 0x0806U && length >= network_offset + 28U ) {
        const uint8_t * arp = frame + network_offset;
        const uint8_t * source_ip = arp + 14U;
        const uint8_t * target_ip = arp + 24U;
        aicp_uart_printf(
            "AICP_FREERTOS_NET_PACKET dir=%s count=%u len=%u kind=arp op=%u src=%u.%u.%u.%u dst=%u.%u.%u.%u\n",
            direction,
            *count,
            ( unsigned int ) length,
            read_be16( arp + 6U ),
            source_ip[0], source_ip[1], source_ip[2], source_ip[3],
            target_ip[0], target_ip[1], target_ip[2], target_ip[3] );
        return;
    }

    if( ether_type == 0x0800U && length >= network_offset + 20U ) {
        const uint8_t * ipv4 = frame + network_offset;
        const size_t ip_header_length = ( size_t ) ( ipv4[0] & 0x0fU ) * 4U;
        const uint8_t protocol = ipv4[9];
        const uint8_t * source_ip = ipv4 + 12U;
        const uint8_t * target_ip = ipv4 + 16U;
        if( ip_header_length >= 20U && length >= network_offset + ip_header_length + 4U &&
            ( protocol == 6U || protocol == 17U ) ) {
            const uint8_t * transport = ipv4 + ip_header_length;
            const uint16_t source_port = read_be16( transport );
            const uint16_t target_port = read_be16( transport + 2U );
            const uint8_t tcp_flags =
                protocol == 6U && length >= network_offset + ip_header_length + 14U
                    ? transport[13]
                    : 0U;
            aicp_uart_printf(
                "AICP_FREERTOS_NET_PACKET dir=%s count=%u len=%u kind=ipv4 proto=%u src=%u.%u.%u.%u:%u dst=%u.%u.%u.%u:%u tcp_flags=%x\n",
                direction,
                *count,
                ( unsigned int ) length,
                protocol,
                source_ip[0], source_ip[1], source_ip[2], source_ip[3], source_port,
                target_ip[0], target_ip[1], target_ip[2], target_ip[3], target_port,
                tcp_flags );
            return;
        }
        aicp_uart_printf(
            "AICP_FREERTOS_NET_PACKET dir=%s count=%u len=%u kind=ipv4 proto=%u src=%u.%u.%u.%u dst=%u.%u.%u.%u\n",
            direction,
            *count,
            ( unsigned int ) length,
            protocol,
            source_ip[0], source_ip[1], source_ip[2], source_ip[3],
            target_ip[0], target_ip[1], target_ip[2], target_ip[3] );
        return;
    }

    aicp_uart_printf(
        "AICP_FREERTOS_NET_PACKET dir=%s count=%u len=%u kind=ethernet ethertype=%x\n",
        direction,
        *count,
        ( unsigned int ) length,
        ether_type );
}

static void receive_task( void * argument )
{
    ( void ) argument;
    aicp_virtio_net_set_rx_task( xTaskGetCurrentTaskHandle() );
    aicp_uart_printf( "AICP_FREERTOS_NET_RX_TASK_READY mode=irq fallback_ms=%u\n",
                      AICP_RX_RECOVERY_MS );

    for( ;; ) {
        const uint32_t notified =
            ulTaskNotifyTake( pdTRUE, pdMS_TO_TICKS( AICP_RX_RECOVERY_MS ) );
        size_t length;
        bool received = false;
        while( aicp_virtio_net_receive( rx_frame, sizeof( rx_frame ), &length ) ) {
            received = true;
            trace_packet( "rx", rx_frame, length, &rx_trace_count );
            if( eConsiderFrameForProcessing( rx_frame ) != eProcessBuffer ) {
                continue;
            }
            NetworkBufferDescriptor_t * descriptor =
                pxGetNetworkBufferWithDescriptor( length, 0U );
            if( descriptor == NULL ) {
                aicp_uart_printf( "AICP_FREERTOS_NET_RX_DROP reason=no_buffer len=%u\n",
                                  ( unsigned int ) length );
                continue;
            }
            memcpy( descriptor->pucEthernetBuffer, rx_frame, length );
            descriptor->xDataLength = length;
            descriptor->pxInterface = network_interface;
            descriptor->pxEndPoint =
                FreeRTOS_MatchingEndpoint( network_interface, descriptor->pucEthernetBuffer );
            IPStackEvent_t event = {
                .eEventType = eNetworkRxEvent,
                .pvData = descriptor,
            };
            if( xSendEventStructToIPTask( &event, 0U ) == pdFALSE ) {
                vReleaseNetworkBufferAndDescriptor( descriptor );
                aicp_uart_puts( "AICP_FREERTOS_NET_RX_DROP reason=ip_queue_full\n" );
            }
        }
        if( received && notified == 0U ) {
            poll_recovery_count++;
            if( poll_recovery_count <= 8U ) {
                aicp_uart_printf( "AICP_FREERTOS_NET_POLL_RECOVERY count=%u\n",
                                  poll_recovery_count );
            }
        }
    }
}

static BaseType_t network_initialise( NetworkInterface_t * interface )
{
    network_interface = interface;
    if( !aicp_virtio_net_init() ) {
        return pdFAIL;
    }
    if( rx_task == NULL ) {
        if( xTaskCreate( receive_task,
                         "virtio-rx",
                         AICP_RX_TASK_STACK_WORDS,
                         NULL,
                         AICP_RX_TASK_PRIORITY,
                         &rx_task ) != pdPASS ) {
            return pdFAIL;
        }
    }
    return pdPASS;
}

static BaseType_t network_output( NetworkInterface_t * interface,
                                  NetworkBufferDescriptor_t * const descriptor,
                                  BaseType_t release_after_send )
{
    ( void ) interface;
    trace_packet( "tx",
                  descriptor->pucEthernetBuffer,
                  descriptor->xDataLength,
                  &tx_trace_count );
    const BaseType_t result =
        aicp_virtio_net_send( descriptor->pucEthernetBuffer, descriptor->xDataLength )
            ? pdTRUE
            : pdFALSE;
    if( release_after_send != pdFALSE ) {
        vReleaseNetworkBufferAndDescriptor( descriptor );
    }
    return result;
}

static BaseType_t network_link_status( NetworkInterface_t * interface )
{
    ( void ) interface;
    return pdTRUE;
}

NetworkInterface_t * pxFillInterfaceDescriptor( BaseType_t index,
                                                NetworkInterface_t * interface )
{
    static char name[] = "virtio0";
    ( void ) index;
    memset( interface, 0, sizeof( *interface ) );
    interface->pcName = name;
    interface->pfInitialise = network_initialise;
    interface->pfOutput = network_output;
    interface->pfGetPhyLinkStatus = network_link_status;
    FreeRTOS_AddNetworkInterface( interface );
    return interface;
}
