/* Copyright 2026 The TGOSKits Authors */
/* SPDX-License-Identifier: Apache-2.0 */

#define AICP_FREERTOS 1

#include "platform.h"

#include "FreeRTOS.h"
#include "FreeRTOS_IP.h"
#include "FreeRTOS_Sockets.h"
#include "task.h"

#include "aicp_service.h"

#include <string.h>

#define AICP_PORT                8800U
#define AICP_SERVER_STACK_WORDS  4096U
#define AICP_SERVER_PRIORITY     5U
#define BASELINE_PERIOD_MS       20U
#define BASELINE_SAMPLE_COUNT    1000U
#define BASELINE_TASK_PRIORITY   6U
#define BASELINE_STACK_WORDS     2048U

static volatile BaseType_t network_up;
static uint32_t random_state = 0x6d2b79f5U;

#ifndef AICP_FREERTOS_BASELINE
static ptrdiff_t freertos_stream_read(
    void * context,
    void * buffer,
    size_t length )
{
    const Socket_t socket = *( const Socket_t * ) context;
    return ( ptrdiff_t ) FreeRTOS_recv( socket, buffer, length, 0 );
}

static ptrdiff_t freertos_stream_write(
    void * context,
    const void * buffer,
    size_t length )
{
    const Socket_t socket = *( const Socket_t * ) context;
    return ( ptrdiff_t ) FreeRTOS_send( socket, buffer, length, 0 );
}
#endif

static uint64_t monotonic_ns( void )
{
    const uint64_t frequency = aicp_counter_frequency();
    return frequency == 0U ? 0U : ( aicp_counter_read() * 1000000000ULL ) / frequency;
}

#ifndef AICP_FREERTOS_BASELINE
static uint64_t service_monotonic_ns( void * context )
{
    ( void ) context;
    return monotonic_ns();
}
#endif

BaseType_t xApplicationGetRandomNumber( uint32_t * value )
{
    uint32_t state = random_state ^ ( uint32_t ) aicp_counter_read();
    state ^= state << 13U;
    state ^= state >> 17U;
    state ^= state << 5U;
    random_state = state;
    *value = state;
    return pdTRUE;
}

uint32_t ulApplicationGetNextSequenceNumber( uint32_t source_address,
                                             uint16_t source_port,
                                             uint32_t destination_address,
                                             uint16_t destination_port )
{
    uint32_t value;
    ( void ) xApplicationGetRandomNumber( &value );
    return value ^ source_address ^ destination_address ^
           ( ( uint32_t ) source_port << 16U ) ^ destination_port;
}

void vApplicationIPNetworkEventHook( eIPCallbackEvent_t event )
{
    network_up = event == eNetworkUp ? pdTRUE : pdFALSE;
    aicp_uart_printf( "AICP_FREERTOS_NETWORK_EVENT state=%s\n",
                      network_up == pdTRUE ? "up" : "down" );
}

#ifndef AICP_FREERTOS_BASELINE
static Socket_t listen_tcp( uint16_t port )
{
    Socket_t socket = FreeRTOS_socket( FREERTOS_AF_INET,
                                      FREERTOS_SOCK_STREAM,
                                      FREERTOS_IPPROTO_TCP );
    if( socket == FREERTOS_INVALID_SOCKET ) {
        return FREERTOS_INVALID_SOCKET;
    }

    struct freertos_sockaddr address;
    memset( &address, 0, sizeof( address ) );
    address.sin_family = FREERTOS_AF_INET;
    address.sin_port = FreeRTOS_htons( port );
    address.sin_addr = 0U;
    if( FreeRTOS_bind( socket, &address, sizeof( address ) ) != 0 ||
        FreeRTOS_listen( socket, 2 ) != 0 ) {
        FreeRTOS_closesocket( socket );
        return FREERTOS_INVALID_SOCKET;
    }
    return socket;
}

static void log_service_event(
    void * context,
    const struct aicp_service_event_data * event )
{
    ( void ) context;
    const struct aicp_header * header = event->header;

    switch( event->event ) {
        case AICP_SERVICE_FRAME_RECEIVED:
            aicp_uart_printf( "AICP_FREERTOS_RX_FRAME type=%u seq=%u len=%u\n",
                              header->msg_type,
                              header->seq,
                              header->payload_len );
            break;
        case AICP_SERVICE_HELLO:
            aicp_uart_printf( "AICP_FREERTOS_HELLO seq=%u\n", header->seq );
            break;
        case AICP_SERVICE_CONTROL_APPLIED:
            aicp_uart_printf(
                "AICP_FREERTOS_CONTROL seq=%u target_milli=%u measured_milli=%u output_milli=%u mode=%u\n",
                header->seq,
                ( unsigned int ) ( event->control->setpoint * 1000.0f ),
                ( unsigned int ) ( event->control->measured * 1000.0f ),
                ( unsigned int ) ( event->control->control_output * 1000.0f ),
                event->control->mode );
            break;
        case AICP_SERVICE_STATUS_SENT:
            aicp_uart_printf( "AICP_FREERTOS_TX_FRAME type=%u seq=%u\n",
                              AICP_MSG_STATUS,
                              header->seq );
            break;
        case AICP_SERVICE_ERROR_SENT:
            aicp_uart_printf( "AICP_FREERTOS_ERROR_NOTIFY seq=%u code=%u\n",
                              header->seq,
                              event->error_code );
            break;
        case AICP_SERVICE_DUPLICATE:
            aicp_uart_printf( "AICP_FREERTOS_DUPLICATE seq=%u\n", header->seq );
            break;
        case AICP_SERVICE_STALE:
            aicp_uart_printf( "AICP_FREERTOS_STALE seq=%u\n", header->seq );
            break;
        case AICP_SERVICE_DISCONNECTED:
            aicp_uart_printf( "AICP_FREERTOS_CLIENT_DISCONNECTED ret=%u\n",
                              ( unsigned int ) ( -event->result ) );
            break;
    }
}

static void serve_client( Socket_t socket )
{
    static struct aicp_service_session session;
    static struct aicp_service_stats stats;
    struct aicp_stream stream = {
        .read = freertos_stream_read,
        .write = freertos_stream_write,
        .context = &socket,
    };
    const struct aicp_service_ops ops = {
        .monotonic_ns = service_monotonic_ns,
        .on_event = log_service_event,
        .context = NULL,
    };

    aicp_service_session_init( &session );
    aicp_uart_puts( "AICP_FREERTOS_CLIENT_CONNECTED\n" );
    ( void ) aicp_service_serve( &stream, &session, &stats, &ops );
    FreeRTOS_closesocket( socket );
}

static void server_task( void * argument )
{
    ( void ) argument;
    portTASK_USES_FLOATING_POINT();
    while( network_up == pdFALSE ) {
        vTaskDelay( pdMS_TO_TICKS( 100U ) );
    }

    Socket_t listener = listen_tcp( AICP_PORT );
    configASSERT( listener != FREERTOS_INVALID_SOCKET );
    aicp_uart_printf( "AICP_FREERTOS_READY transport=tcp port=%u ip=10.0.3.2\n",
                      AICP_PORT );
    for( ;; ) {
        Socket_t client = FreeRTOS_accept( listener, NULL, NULL );
        if( client == NULL || client == FREERTOS_INVALID_SOCKET ) {
            aicp_uart_puts( "AICP_FREERTOS_ACCEPT_RETRY\n" );
            vTaskDelay( pdMS_TO_TICKS( 100U ) );
            continue;
        }
        serve_client( client );
    }
}
#endif

#ifdef AICP_FREERTOS_BASELINE
static uint64_t abs_diff_u64( uint64_t lhs, uint64_t rhs )
{
    return lhs >= rhs ? lhs - rhs : rhs - lhs;
}

static void sort_u64( uint64_t * values, size_t count )
{
    for( size_t index = 1U; index < count; index++ ) {
        const uint64_t value = values[ index ];
        size_t position = index;
        while( position > 0U && values[ position - 1U ] > value ) {
            values[ position ] = values[ position - 1U ];
            position--;
        }
        values[ position ] = value;
    }
}

static void baseline_periodic_task( void * argument )
{
    ( void ) argument;
    static uint64_t abs_jitter_ns[ BASELINE_SAMPLE_COUNT ];
    const uint64_t period_ns = BASELINE_PERIOD_MS * 1000000ULL;
    TickType_t wake = xTaskGetTickCount();
    uint64_t expected_ns = monotonic_ns() + period_ns;
    uint64_t jitter_sum = 0U;
    uint64_t max_jitter = 0U;
    uint32_t missed_deadlines = 0U;

    aicp_uart_printf(
        "AICP_FREERTOS_BASELINE_START mode=%s samples=%u period_ns=%lu\n",
#ifdef AICP_FREERTOS_STRESS
        "stress",
#else
        "idle",
#endif
        BASELINE_SAMPLE_COUNT,
        ( unsigned long ) period_ns );

    for( size_t index = 0U; index < BASELINE_SAMPLE_COUNT; index++ ) {
        vTaskDelayUntil( &wake, pdMS_TO_TICKS( BASELINE_PERIOD_MS ) );
        const uint64_t now_ns = monotonic_ns();
        const uint64_t jitter_ns = abs_diff_u64( now_ns, expected_ns );
        abs_jitter_ns[ index ] = jitter_ns;
        jitter_sum += jitter_ns;
        if( jitter_ns > max_jitter ) {
            max_jitter = jitter_ns;
        }
        if( now_ns > expected_ns + period_ns ) {
            missed_deadlines++;
        }
        expected_ns += period_ns;
    }

    sort_u64( abs_jitter_ns, BASELINE_SAMPLE_COUNT );
    const size_t p99_index =
        ( ( BASELINE_SAMPLE_COUNT * 99U ) + 99U ) / 100U - 1U;
    aicp_uart_printf(
        "AICP_FREERTOS_BASELINE_DONE mode=%s samples=%u period_ns=%lu "
        "avg_abs_jitter_ns=%lu p99_abs_jitter_ns=%lu "
        "max_abs_jitter_ns=%lu missed_deadlines=%u\n",
#ifdef AICP_FREERTOS_STRESS
        "stress",
#else
        "idle",
#endif
        BASELINE_SAMPLE_COUNT,
        ( unsigned long ) period_ns,
        ( unsigned long ) ( jitter_sum / BASELINE_SAMPLE_COUNT ),
        ( unsigned long ) abs_jitter_ns[ p99_index ],
        ( unsigned long ) max_jitter,
        missed_deadlines );
    vTaskSuspend( NULL );
}

#ifdef AICP_FREERTOS_STRESS
static void baseline_stress_task( void * argument )
{
    volatile uint64_t state = ( uintptr_t ) argument + 1U;
    for( ;; ) {
        for( uint32_t index = 0U; index < 200000U; index++ ) {
            state = ( state * 6364136223846793005ULL ) + 1442695040888963407ULL;
        }
        taskYIELD();
    }
}
#endif
#else

static void periodic_task( void * argument )
{
    ( void ) argument;
    TickType_t wake = xTaskGetTickCount();
    uint32_t count = 0;
    for( ;; ) {
        vTaskDelayUntil( &wake, pdMS_TO_TICKS( 100U ) );
        count++;
        if( count <= 5U || ( count % 10U ) == 0U ) {
            aicp_uart_printf( "AICP_FREERTOS_TICK count=%u tick=%u\n",
                              count,
                              ( unsigned int ) xTaskGetTickCount() );
        }
    }
}
#endif

#ifndef AICP_FREERTOS_BASELINE
static void worker_task( void * argument )
{
    ( void ) argument;
    uint32_t count = 0;
    for( ;; ) {
        vTaskDelay( pdMS_TO_TICKS( 250U ) );
        count++;
        if( count <= 4U || ( count % 20U ) == 0U ) {
            aicp_uart_printf( "AICP_FREERTOS_TASK_SWITCH count=%u tick=%u\n",
                              count,
                              ( unsigned int ) xTaskGetTickCount() );
        }
    }
}
#endif

int main( void )
{
#ifndef AICP_FREERTOS_BASELINE
    static const uint8_t ip_address[ 4 ] = { 10U, 0U, 3U, 2U };
    static const uint8_t netmask[ 4 ] = { 255U, 255U, 255U, 0U };
    static const uint8_t gateway[ 4 ] = { 0U, 0U, 0U, 0U };
    static const uint8_t dns[ 4 ] = { 0U, 0U, 0U, 0U };
    static const uint8_t mac[ 6 ] = { 0x52U, 0x54U, 0x00U, 0xaaU, 0x03U, 0x02U };
#endif

    configASSERT( aicp_platform_init() );
    aicp_uart_puts( "AICP_FREERTOS_SCHEDULER_START tick_hz=1000\n" );
#ifdef AICP_FREERTOS_BASELINE
    configASSERT( xTaskCreate( baseline_periodic_task,
                               "periodic-baseline",
                               BASELINE_STACK_WORDS,
                               NULL,
                               BASELINE_TASK_PRIORITY,
                               NULL ) == pdPASS );
#ifdef AICP_FREERTOS_STRESS
    configASSERT( xTaskCreate( baseline_stress_task,
                               "stress-0",
                               512U,
                               ( void * ) 0U,
                               3U,
                               NULL ) == pdPASS );
    configASSERT( xTaskCreate( baseline_stress_task,
                               "stress-1",
                               512U,
                               ( void * ) 1U,
                               3U,
                               NULL ) == pdPASS );
#endif
#else
    configASSERT( FreeRTOS_IPInit( ip_address, netmask, gateway, dns, mac ) == pdPASS );
    configASSERT( xTaskCreate( server_task,
                               "aicp-server",
                               AICP_SERVER_STACK_WORDS,
                               NULL,
                               AICP_SERVER_PRIORITY,
                               NULL ) == pdPASS );
    configASSERT( xTaskCreate( periodic_task, "periodic", 1024U, NULL, 4U, NULL ) == pdPASS );
    configASSERT( xTaskCreate( worker_task, "worker", 512U, NULL, 3U, NULL ) == pdPASS );
#endif
    vTaskStartScheduler();
    aicp_assert_failed( "scheduler-returned", 0U );
    return 0;
}
