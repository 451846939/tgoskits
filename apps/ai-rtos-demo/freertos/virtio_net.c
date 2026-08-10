/* Copyright 2026 The TGOSKits Authors */
/* SPDX-License-Identifier: Apache-2.0 */

#include "virtio_net.h"

#include "FreeRTOS.h"
#include "platform.h"
#include "task.h"

#include <string.h>

#define VIRTIO_MMIO_BASE                 0x0a003a00UL
#define VIRTIO_MMIO_MAGIC_VALUE          0x000U
#define VIRTIO_MMIO_VERSION              0x004U
#define VIRTIO_MMIO_DEVICE_ID            0x008U
#define VIRTIO_MMIO_DEVICE_FEATURES      0x010U
#define VIRTIO_MMIO_DEVICE_FEATURES_SEL  0x014U
#define VIRTIO_MMIO_DRIVER_FEATURES      0x020U
#define VIRTIO_MMIO_DRIVER_FEATURES_SEL  0x024U
#define VIRTIO_MMIO_GUEST_PAGE_SIZE      0x028U
#define VIRTIO_MMIO_QUEUE_SEL            0x030U
#define VIRTIO_MMIO_QUEUE_NUM_MAX        0x034U
#define VIRTIO_MMIO_QUEUE_NUM            0x038U
#define VIRTIO_MMIO_QUEUE_ALIGN          0x03cU
#define VIRTIO_MMIO_QUEUE_PFN            0x040U
#define VIRTIO_MMIO_QUEUE_NOTIFY         0x050U
#define VIRTIO_MMIO_INTERRUPT_STATUS     0x060U
#define VIRTIO_MMIO_INTERRUPT_ACK        0x064U
#define VIRTIO_MMIO_STATUS               0x070U
#define VIRTIO_MMIO_CONFIG               0x100U

#define VIRTIO_MAGIC                     0x74726976U
#define VIRTIO_LEGACY_VERSION            1U
#define VIRTIO_DEVICE_NET                1U
#define VIRTIO_STATUS_ACKNOWLEDGE         1U
#define VIRTIO_STATUS_DRIVER              2U
#define VIRTIO_STATUS_DRIVER_OK           4U
#define VIRTIO_STATUS_FEATURES_OK         8U
#define VIRTIO_NET_F_MAC                  5U
#define VIRTQ_DESC_F_WRITE                2U
#define VIRTIO_PAGE_SIZE                  4096U
#define VIRTIO_QUEUE_SIZE                 16U
#define VIRTIO_RING_BYTES                 ( 2U * VIRTIO_PAGE_SIZE )
#define VIRTIO_NET_HEADER_SIZE            10U
#define VIRTIO_MAX_FRAME_SIZE             1536U
#define VIRTIO_BUFFER_SIZE                ( VIRTIO_NET_HEADER_SIZE + VIRTIO_MAX_FRAME_SIZE )
#define VIRTIO_RX_QUEUE                   0U
#define VIRTIO_TX_QUEUE                   1U
#define VIRTIO_TRACE_LIMIT                8U

struct virtq_desc {
    uint64_t addr;
    uint32_t len;
    uint16_t flags;
    uint16_t next;
} __attribute__( ( packed ) );

struct virtq_used_elem {
    uint32_t id;
    uint32_t len;
} __attribute__( ( packed ) );

struct virtqueue {
    uint8_t * area;
    struct virtq_desc * desc;
    volatile uint16_t * avail_flags;
    volatile uint16_t * avail_idx;
    volatile uint16_t * avail_ring;
    volatile uint16_t * used_flags;
    volatile uint16_t * used_idx;
    volatile struct virtq_used_elem * used_ring;
    uint16_t last_used;
};

static uint8_t rx_ring_area[VIRTIO_RING_BYTES]
    __attribute__( ( aligned( VIRTIO_PAGE_SIZE ) ) );
static uint8_t tx_ring_area[VIRTIO_RING_BYTES]
    __attribute__( ( aligned( VIRTIO_PAGE_SIZE ) ) );
static uint8_t rx_buffers[VIRTIO_QUEUE_SIZE][VIRTIO_BUFFER_SIZE]
    __attribute__( ( aligned( 64 ) ) );
static uint8_t tx_buffer[VIRTIO_BUFFER_SIZE] __attribute__( ( aligned( 64 ) ) );
static struct virtqueue rx_queue;
static struct virtqueue tx_queue;
static TaskHandle_t rx_task;
static bool device_ready;
static uint32_t irq_count;
static uint32_t rx_count;
static uint32_t tx_count;

static inline uint32_t mmio_read32( uint32_t offset )
{
    __asm volatile( "dmb ish" ::: "memory" );
    return *( volatile uint32_t * )( VIRTIO_MMIO_BASE + offset );
}

static inline void mmio_write32( uint32_t offset, uint32_t value )
{
    *( volatile uint32_t * )( VIRTIO_MMIO_BASE + offset ) = value;
    __asm volatile( "dmb ish" ::: "memory" );
}

static inline void publish( void )
{
    __asm volatile( "dmb ishst" ::: "memory" );
}

static inline void observe( void )
{
    __asm volatile( "dmb ishld" ::: "memory" );
}

static void queue_layout( struct virtqueue * queue, uint8_t * area )
{
    memset( area, 0, VIRTIO_RING_BYTES );
    queue->area = area;
    queue->desc = ( struct virtq_desc * ) area;
    queue->avail_flags = ( volatile uint16_t * )( area + sizeof( struct virtq_desc ) * VIRTIO_QUEUE_SIZE );
    queue->avail_idx = queue->avail_flags + 1;
    queue->avail_ring = queue->avail_idx + 1;
    queue->used_flags = ( volatile uint16_t * )( area + VIRTIO_PAGE_SIZE );
    queue->used_idx = queue->used_flags + 1;
    queue->used_ring = ( volatile struct virtq_used_elem * )( queue->used_idx + 1 );
    queue->last_used = 0U;
}

static bool queue_activate( uint32_t index, struct virtqueue * queue )
{
    mmio_write32( VIRTIO_MMIO_QUEUE_SEL, index );
    const uint32_t maximum = mmio_read32( VIRTIO_MMIO_QUEUE_NUM_MAX );
    if( maximum < VIRTIO_QUEUE_SIZE || mmio_read32( VIRTIO_MMIO_QUEUE_PFN ) != 0U ) {
        aicp_uart_printf( "AICP_FREERTOS_VIRTIO_QUEUE_FAIL queue=%u max=%u pfn=%x\n",
                          index,
                          maximum,
                          mmio_read32( VIRTIO_MMIO_QUEUE_PFN ) );
        return false;
    }
    mmio_write32( VIRTIO_MMIO_QUEUE_NUM, VIRTIO_QUEUE_SIZE );
    mmio_write32( VIRTIO_MMIO_QUEUE_ALIGN, VIRTIO_PAGE_SIZE );
    mmio_write32( VIRTIO_MMIO_QUEUE_PFN, ( uint32_t )( ( uintptr_t ) queue->area >> 12U ) );
    aicp_uart_printf( "AICP_FREERTOS_VIRTIO_QUEUE queue=%u size=%u pfn=%x\n",
                      index,
                      VIRTIO_QUEUE_SIZE,
                      mmio_read32( VIRTIO_MMIO_QUEUE_PFN ) );
    return true;
}

static void rx_publish_descriptor( uint16_t descriptor )
{
    const uint16_t avail = *rx_queue.avail_idx;
    rx_queue.avail_ring[avail % VIRTIO_QUEUE_SIZE] = descriptor;
    publish();
    *rx_queue.avail_idx = ( uint16_t )( avail + 1U );
}

bool aicp_virtio_net_init( void )
{
    if( device_ready ) {
        return true;
    }
    if( mmio_read32( VIRTIO_MMIO_MAGIC_VALUE ) != VIRTIO_MAGIC ||
        mmio_read32( VIRTIO_MMIO_VERSION ) != VIRTIO_LEGACY_VERSION ||
        mmio_read32( VIRTIO_MMIO_DEVICE_ID ) != VIRTIO_DEVICE_NET ) {
        aicp_uart_printf( "AICP_FREERTOS_VIRTIO_PROBE_FAIL magic=%x version=%u device=%u\n",
                          mmio_read32( VIRTIO_MMIO_MAGIC_VALUE ),
                          mmio_read32( VIRTIO_MMIO_VERSION ),
                          mmio_read32( VIRTIO_MMIO_DEVICE_ID ) );
        return false;
    }

    mmio_write32( VIRTIO_MMIO_STATUS, 0U );
    mmio_write32( VIRTIO_MMIO_STATUS, VIRTIO_STATUS_ACKNOWLEDGE );
    mmio_write32( VIRTIO_MMIO_STATUS, VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER );
    mmio_write32( VIRTIO_MMIO_DEVICE_FEATURES_SEL, 0U );
    const uint32_t offered = mmio_read32( VIRTIO_MMIO_DEVICE_FEATURES );
    const uint32_t accepted = offered & ( 1U << VIRTIO_NET_F_MAC );
    mmio_write32( VIRTIO_MMIO_DRIVER_FEATURES_SEL, 0U );
    mmio_write32( VIRTIO_MMIO_DRIVER_FEATURES, accepted );
    mmio_write32( VIRTIO_MMIO_STATUS,
                  VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER | VIRTIO_STATUS_FEATURES_OK );
    if( ( mmio_read32( VIRTIO_MMIO_STATUS ) & VIRTIO_STATUS_FEATURES_OK ) == 0U ) {
        aicp_uart_printf( "AICP_FREERTOS_VIRTIO_FEATURE_FAIL offered=%x accepted=%x\n",
                          offered,
                          accepted );
        return false;
    }

    mmio_write32( VIRTIO_MMIO_GUEST_PAGE_SIZE, VIRTIO_PAGE_SIZE );
    queue_layout( &rx_queue, rx_ring_area );
    queue_layout( &tx_queue, tx_ring_area );
    if( !queue_activate( VIRTIO_RX_QUEUE, &rx_queue ) ||
        !queue_activate( VIRTIO_TX_QUEUE, &tx_queue ) ) {
        return false;
    }

    for( uint16_t i = 0U; i < VIRTIO_QUEUE_SIZE; i++ ) {
        rx_queue.desc[i].addr = ( uintptr_t ) rx_buffers[i];
        rx_queue.desc[i].len = VIRTIO_BUFFER_SIZE;
        rx_queue.desc[i].flags = VIRTQ_DESC_F_WRITE;
        rx_queue.desc[i].next = 0U;
        rx_publish_descriptor( i );
    }
    publish();
    mmio_write32( VIRTIO_MMIO_QUEUE_NOTIFY, VIRTIO_RX_QUEUE );

    mmio_write32( VIRTIO_MMIO_STATUS,
                  VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER |
                      VIRTIO_STATUS_FEATURES_OK | VIRTIO_STATUS_DRIVER_OK );
    aicp_platform_enable_net_irq();
    device_ready = true;
    aicp_uart_printf(
        "AICP_FREERTOS_VIRTIO_READY base=%p mac=%x:%x:%x:%x:%x:%x features=%x\n",
        ( void * ) VIRTIO_MMIO_BASE,
        *( volatile uint8_t * )( VIRTIO_MMIO_BASE + VIRTIO_MMIO_CONFIG + 0U ),
        *( volatile uint8_t * )( VIRTIO_MMIO_BASE + VIRTIO_MMIO_CONFIG + 1U ),
        *( volatile uint8_t * )( VIRTIO_MMIO_BASE + VIRTIO_MMIO_CONFIG + 2U ),
        *( volatile uint8_t * )( VIRTIO_MMIO_BASE + VIRTIO_MMIO_CONFIG + 3U ),
        *( volatile uint8_t * )( VIRTIO_MMIO_BASE + VIRTIO_MMIO_CONFIG + 4U ),
        *( volatile uint8_t * )( VIRTIO_MMIO_BASE + VIRTIO_MMIO_CONFIG + 5U ),
        accepted );
    return true;
}

void aicp_virtio_net_set_rx_task( void * task )
{
    rx_task = ( TaskHandle_t ) task;
}

void aicp_virtio_net_isr( void )
{
    const uint32_t status = mmio_read32( VIRTIO_MMIO_INTERRUPT_STATUS );
    if( status != 0U ) {
        mmio_write32( VIRTIO_MMIO_INTERRUPT_ACK, status );
    }
    irq_count++;
    if( irq_count <= VIRTIO_TRACE_LIMIT ) {
        aicp_uart_printf( "AICP_FREERTOS_VIRTIO_IRQ count=%u status=%x\n", irq_count, status );
    }
    if( rx_task != NULL ) {
        BaseType_t higher_priority_task_woken = pdFALSE;
        vTaskNotifyGiveFromISR( rx_task, &higher_priority_task_woken );
        portYIELD_FROM_ISR( higher_priority_task_woken );
    }
}

bool aicp_virtio_net_receive( uint8_t * frame, size_t capacity, size_t * length )
{
    observe();
    if( rx_queue.last_used == *rx_queue.used_idx ) {
        return false;
    }
    const struct virtq_used_elem used =
        rx_queue.used_ring[rx_queue.last_used % VIRTIO_QUEUE_SIZE];
    rx_queue.last_used++;
    if( used.id >= VIRTIO_QUEUE_SIZE || used.len <= VIRTIO_NET_HEADER_SIZE ) {
        aicp_uart_printf( "AICP_FREERTOS_VIRTIO_RX_BAD id=%u len=%u\n", used.id, used.len );
        return false;
    }
    size_t frame_length = used.len - VIRTIO_NET_HEADER_SIZE;
    if( frame_length > capacity ) {
        frame_length = capacity;
    }
    memcpy( frame, rx_buffers[used.id] + VIRTIO_NET_HEADER_SIZE, frame_length );
    rx_publish_descriptor( ( uint16_t ) used.id );
    publish();
    mmio_write32( VIRTIO_MMIO_QUEUE_NOTIFY, VIRTIO_RX_QUEUE );
    *length = frame_length;
    rx_count++;
    if( rx_count <= VIRTIO_TRACE_LIMIT ) {
        aicp_uart_printf( "AICP_FREERTOS_VIRTIO_RX count=%u len=%u id=%u\n",
                          rx_count,
                          ( unsigned int ) frame_length,
                          used.id );
    }
    return true;
}

bool aicp_virtio_net_send( const uint8_t * frame, size_t length )
{
    if( !device_ready || length > VIRTIO_MAX_FRAME_SIZE ) {
        return false;
    }
    memset( tx_buffer, 0, VIRTIO_NET_HEADER_SIZE );
    memcpy( tx_buffer + VIRTIO_NET_HEADER_SIZE, frame, length );
    tx_queue.desc[0].addr = ( uintptr_t ) tx_buffer;
    tx_queue.desc[0].len = ( uint32_t )( VIRTIO_NET_HEADER_SIZE + length );
    tx_queue.desc[0].flags = 0U;
    tx_queue.desc[0].next = 0U;
    const uint16_t avail = *tx_queue.avail_idx;
    tx_queue.avail_ring[avail % VIRTIO_QUEUE_SIZE] = 0U;
    publish();
    *tx_queue.avail_idx = ( uint16_t )( avail + 1U );
    publish();
    mmio_write32( VIRTIO_MMIO_QUEUE_NOTIFY, VIRTIO_TX_QUEUE );

    const TickType_t deadline = xTaskGetTickCount() + pdMS_TO_TICKS( 100U );
    while( tx_queue.last_used == *tx_queue.used_idx ) {
        observe();
        if( xTaskGetTickCount() >= deadline ) {
            aicp_uart_printf( "AICP_FREERTOS_VIRTIO_TX_TIMEOUT avail=%u used=%u\n",
                              *tx_queue.avail_idx,
                              *tx_queue.used_idx );
            return false;
        }
        taskYIELD();
    }
    tx_queue.last_used++;
    tx_count++;
    if( tx_count <= VIRTIO_TRACE_LIMIT ) {
        aicp_uart_printf( "AICP_FREERTOS_VIRTIO_TX count=%u len=%u\n",
                          tx_count,
                          ( unsigned int ) length );
    }
    return true;
}
