/* Copyright 2026 The TGOSKits Authors */
/* SPDX-License-Identifier: Apache-2.0 */

#include "platform.h"

#include "FreeRTOS.h"
#include "task.h"

#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>

#define PL011_BASE              0x09000000UL
#define PL011_DR                0x000UL
#define GICR_BASE               0x080A0000UL
#define GICR_SGI_BASE           ( GICR_BASE + 0x10000UL )
#define GICR_WAKER              0x0014UL
#define GICR_WAKER_PROCESSOR_SLEEP ( 1U << 1 )
#define GICR_WAKER_CHILDREN_ASLEEP ( 1U << 2 )
#define GICD_BASE               0x08000000UL
#define GICD_CTLR               0x0000UL
#define GICD_CTLR_ENABLE_NS     0x0013U
#define GICD_CTLR_RWP           ( 1U << 31 )
#define GIC_IGROUPR0            0x0080UL
#define GIC_ISENABLER0          0x0100UL
#define GIC_ICPENDR0            0x0280UL
#define GIC_IPRIORITYR          0x0400UL
#define GIC_ICFGR1              0x0C04UL
#define GIC_IGROUPMODR0         0x0D00UL
#define TIMER_IRQ_ID            27U
#define VIRTIO_NET_IRQ_ID       77U
#define GIC_PRIORITY_SHIFT      3U
#define GIC_LOWEST_USABLE_PRIO  ( ( configUNIQUE_INTERRUPT_PRIORITIES - 2U ) << GIC_PRIORITY_SHIFT )
#define NSEC_PER_SEC            1000000000ULL

static const void * boot_dtb;
static uint64_t tick_cycles;
static uint64_t next_tick_deadline;

extern void FreeRTOS_Tick_Handler( void );

static inline void mmio_write32( uintptr_t address, uint32_t value )
{
    *( volatile uint32_t * ) address = value;
}

static inline uint32_t mmio_read32( uintptr_t address )
{
    return *( volatile uint32_t * ) address;
}

uint64_t aicp_counter_read( void )
{
    uint64_t value;
    __asm volatile( "mrs %0, cntvct_el0" : "=r"( value ) );
    return value;
}

uint64_t aicp_counter_frequency( void )
{
    uint64_t value;
    __asm volatile( "mrs %0, cntfrq_el0" : "=r"( value ) );
    return value;
}

static void write_cntv_cval( uint64_t value )
{
    __asm volatile( "msr cntv_cval_el0, %0" :: "r"( value ) );
}

static void write_cntv_ctl( uint64_t value )
{
    __asm volatile( "msr cntv_ctl_el0, %0" :: "r"( value ) );
}

static uint64_t read_cntv_ctl( void )
{
    uint64_t value;
    __asm volatile( "mrs %0, cntv_ctl_el0" : "=r"( value ) );
    return value;
}

static uint64_t read_cntv_tval( void )
{
    uint64_t value;
    __asm volatile( "mrs %0, cntv_tval_el0" : "=r"( value ) );
    return value;
}

static uint64_t read_cntv_cval( void )
{
    uint64_t value;
    __asm volatile( "mrs %0, cntv_cval_el0" : "=r"( value ) );
    return value;
}

static uint64_t read_icc_rpr( void )
{
    uint64_t value;
    __asm volatile( "mrs %0, S3_0_C12_C11_3" : "=r"( value ) );
    return value;
}

static uint64_t read_icc_ctlr( void )
{
    uint64_t value;
    __asm volatile( "mrs %0, S3_0_C12_C12_4" : "=r"( value ) );
    return value;
}

void aicp_platform_set_dtb( const void * dtb )
{
    boot_dtb = dtb;
}

void aicp_uart_putc( char value )
{
    if( value == '\n' ) {
        mmio_write32( PL011_BASE + PL011_DR, '\r' );
    }
    mmio_write32( PL011_BASE + PL011_DR, ( uint32_t ) value );
    __asm volatile( "dsb sy" ::: "memory" );
}

void aicp_uart_puts( const char * value )
{
    while( *value != '\0' ) {
        aicp_uart_putc( *value++ );
    }
}

static void print_unsigned( uint64_t value, unsigned int base )
{
    static const char digits[] = "0123456789abcdef";
    char buffer[32];
    size_t length = 0;
    do {
        buffer[length++] = digits[value % base];
        value /= base;
    } while( value != 0U );
    while( length != 0U ) {
        aicp_uart_putc( buffer[--length] );
    }
}

void aicp_uart_printf( const char * format, ... )
{
    va_list args;
    va_start( args, format );
    while( *format != '\0' ) {
        if( *format++ != '%' ) {
            aicp_uart_putc( format[-1] );
            continue;
        }
        bool long_value = false;
        if( *format == 'l' ) {
            long_value = true;
            format++;
            if( *format == 'l' ) {
                format++;
            }
        }
        switch( *format++ ) {
            case 's':
                aicp_uart_puts( va_arg( args, const char * ) );
                break;
            case 'u':
                print_unsigned( long_value ? va_arg( args, unsigned long ) : va_arg( args, unsigned int ), 10U );
                break;
            case 'x':
                print_unsigned( long_value ? va_arg( args, unsigned long ) : va_arg( args, unsigned int ), 16U );
                break;
            case 'p':
                aicp_uart_puts( "0x" );
                print_unsigned( ( uintptr_t ) va_arg( args, void * ), 16U );
                break;
            case '%':
                aicp_uart_putc( '%' );
                break;
            default:
                aicp_uart_putc( '?' );
                break;
        }
    }
    va_end( args );
}

void aicp_delay_us( uint32_t usec )
{
    const uint64_t deadline = aicp_counter_read() +
                              ( aicp_counter_frequency() * usec ) / 1000000ULL;
    while( aicp_counter_read() < deadline ) {
        __asm volatile( "yield" );
    }
}

static void gic_init( void )
{
    const uint32_t timer_bit = 1U << TIMER_IRQ_ID;
    uint64_t sre;
    uint64_t zero = 0U;
#if defined( AICP_FREERTOS_BASELINE )
    uint32_t ctlr;
    uint32_t waker;

    /*
     * A standalone, firmware-less QEMU payload owns the complete GIC.  An
     * AxVisor guest does not execute this block because the host already owns
     * and initializes the shared Distributor and Redistributor lifecycle.
     */
    ctlr = mmio_read32( GICD_BASE + GICD_CTLR );
    mmio_write32( GICD_BASE + GICD_CTLR, ctlr | GICD_CTLR_ENABLE_NS );
    while( ( mmio_read32( GICD_BASE + GICD_CTLR ) & GICD_CTLR_RWP ) != 0U ) {
        __asm volatile( "yield" );
    }

    waker = mmio_read32( GICR_BASE + GICR_WAKER );
    mmio_write32( GICR_BASE + GICR_WAKER, waker & ~GICR_WAKER_PROCESSOR_SLEEP );
    while( ( mmio_read32( GICR_BASE + GICR_WAKER ) & GICR_WAKER_CHILDREN_ASLEEP ) != 0U ) {
        __asm volatile( "yield" );
    }
    aicp_uart_printf( "AICP_FREERTOS_GIC_OWNER ctlr=%x waker=%x\n",
                      mmio_read32( GICD_BASE + GICD_CTLR ),
                      mmio_read32( GICR_BASE + GICR_WAKER ) );
#endif

    __asm volatile( "mrs %0, S3_0_C12_C12_5" : "=r"( sre ) );
    sre |= 0x7U;
    __asm volatile( "msr S3_0_C12_C12_5, %0" :: "r"( sre ) );
    /*
     * The FreeRTOS SRE port completes an IRQ with ICC_EOIR1_EL1 only.
     * Clear ICC_CTLR_EL1.EOImode so EOI also deactivates the interrupt;
     * otherwise a level-triggered timer PPI remains active after its first tick.
     */
    __asm volatile( "msr S3_0_C12_C12_4, %0" :: "r"( zero ) );
    __asm volatile( "msr S3_0_C12_C12_3, %0" :: "r"( zero ) );
    __asm volatile( "msr S3_0_C4_C6_0, %0" :: "r"( 0xffULL ) );
    __asm volatile( "msr S3_0_C12_C12_7, %0" :: "r"( 1ULL ) );

    /*
     * The physical redistributor is shared with the EL2 host in passthrough
     * mode. Configure only the PPI owned by this guest and preserve every
     * other SGI/PPI's group and trigger state. In particular, rewriting the
     * whole registers would move a host-owned scheduling timer into the
     * guest's Group 1 interrupt stream.
     */
    mmio_write32( GICR_SGI_BASE + GIC_IGROUPR0,
                  mmio_read32( GICR_SGI_BASE + GIC_IGROUPR0 ) | timer_bit );
    mmio_write32( GICR_SGI_BASE + GIC_IGROUPMODR0,
                  mmio_read32( GICR_SGI_BASE + GIC_IGROUPMODR0 ) | timer_bit );
    mmio_write32( GICR_SGI_BASE + GIC_ICFGR1,
                  mmio_read32( GICR_SGI_BASE + GIC_ICFGR1 ) &
                      ~( 3U << ( ( TIMER_IRQ_ID - 16U ) * 2U ) ) );
    mmio_write32( GICR_SGI_BASE + GIC_ICPENDR0, timer_bit );
    *( volatile uint8_t * )( GICR_SGI_BASE + GIC_IPRIORITYR + TIMER_IRQ_ID ) =
        ( uint8_t ) GIC_LOWEST_USABLE_PRIO;
    mmio_write32( GICR_SGI_BASE + GIC_ISENABLER0, timer_bit );
    __asm volatile( "dsb sy; isb" ::: "memory" );
    aicp_uart_printf( "AICP_FREERTOS_GIC ctlr=%lx sre=%lx\n",
                      ( unsigned long ) read_icc_ctlr(),
                      ( unsigned long ) sre );
}

void aicp_platform_enable_net_irq( void )
{
    const uint32_t bit = 1U << ( VIRTIO_NET_IRQ_ID % 32U );
    const uintptr_t group = GICD_BASE + GIC_IGROUPR0 + 4U * ( VIRTIO_NET_IRQ_ID / 32U );
    const uintptr_t enable = GICD_BASE + GIC_ISENABLER0 + 4U * ( VIRTIO_NET_IRQ_ID / 32U );
    const uintptr_t pending = GICD_BASE + GIC_ICPENDR0 + 4U * ( VIRTIO_NET_IRQ_ID / 32U );
    const uintptr_t config = GICD_BASE + 0x0c00U + 4U * ( VIRTIO_NET_IRQ_ID / 16U );
    const uint32_t shift = ( VIRTIO_NET_IRQ_ID % 16U ) * 2U;

    mmio_write32( group, mmio_read32( group ) | bit );
    mmio_write32( config, mmio_read32( config ) & ~( 3U << shift ) );
    mmio_write32( pending, bit );
    *( volatile uint8_t * )( GICD_BASE + GIC_IPRIORITYR + VIRTIO_NET_IRQ_ID ) =
        ( uint8_t ) GIC_LOWEST_USABLE_PRIO;
    mmio_write32( enable, bit );
    __asm volatile( "dsb sy; isb" ::: "memory" );
    aicp_uart_printf( "AICP_FREERTOS_NET_IRQ_ENABLED intid=%u priority=%u\n",
                      VIRTIO_NET_IRQ_ID,
                      ( unsigned int ) GIC_LOWEST_USABLE_PRIO );
}

void aicp_platform_init( void )
{
    ( void ) boot_dtb;
    aicp_uart_puts( "AICP_FREERTOS_BOOT platform=qemu-virt arch=aarch64\n" );
    gic_init();
}

void vConfigureTickInterrupt( void )
{
    tick_cycles = aicp_counter_frequency() / configTICK_RATE_HZ;
    next_tick_deadline = aicp_counter_read() + tick_cycles;
    write_cntv_cval( next_tick_deadline );
    write_cntv_ctl( 1U );
    __asm volatile( "dsb sy; isb" ::: "memory" );
    aicp_uart_printf( "AICP_FREERTOS_TIMER_SETUP cycles=%lu ctl=%lx tval=%lx cval=%lx\n",
                      ( unsigned long ) tick_cycles,
                      ( unsigned long ) read_cntv_ctl(),
                      ( unsigned long ) read_cntv_tval(),
                      ( unsigned long ) read_cntv_cval() );
}

void vClearTickInterrupt( void )
{
    static uint32_t clear_count;
    static uint32_t overrun_count;
    uint64_t now;
    clear_count++;
    if( clear_count <= 3U ) {
        aicp_uart_printf( "AICP_FREERTOS_TIMER_REARM_BEGIN count=%u\n", clear_count );
    }
    /*
     * Advance from the previous absolute deadline.  Rearming TVAL from the
     * end of the handler accumulates interrupt-service time into every tick
     * and creates unbounded phase drift under TCG or host load.
     */
    next_tick_deadline += tick_cycles;
    now = aicp_counter_read();
    if( next_tick_deadline <= now ) {
        const uint64_t skipped_periods =
            ( ( now - next_tick_deadline ) / tick_cycles ) + 1U;
        next_tick_deadline += skipped_periods * tick_cycles;
        overrun_count++;
        if( overrun_count <= 8U ) {
            aicp_uart_printf(
                "AICP_FREERTOS_TIMER_OVERRUN count=%u skipped_periods=%lu\n",
                overrun_count,
                ( unsigned long ) skipped_periods );
        }
    }
    write_cntv_cval( next_tick_deadline );
    write_cntv_ctl( 1U );
    __asm volatile( "dsb sy; isb" ::: "memory" );
    if( clear_count <= 3U ) {
        aicp_uart_printf(
            "AICP_FREERTOS_TIMER_REARM_END count=%u ctl=%lx tval=%lx cval=%lx rpr=%lx\n",
            clear_count,
            ( unsigned long ) read_cntv_ctl(),
            ( unsigned long ) read_cntv_tval(),
            ( unsigned long ) read_cntv_cval(),
            ( unsigned long ) read_icc_rpr() );
    }
}

void vApplicationIRQHandler( uint32_t iar )
{
    static uint32_t timer_irq_count;
    static uint32_t unexpected_irq_count;
    const uint32_t intid = iar & 0x00ffffffU;
    if( intid == TIMER_IRQ_ID ) {
        timer_irq_count++;
        if( timer_irq_count <= 3U ) {
            aicp_uart_printf( "AICP_FREERTOS_TIMER_IRQ count=%u iar=%x\n",
                              timer_irq_count,
                              iar );
        }
        FreeRTOS_Tick_Handler();
        if( timer_irq_count <= 3U ) {
            aicp_uart_printf( "AICP_FREERTOS_TIMER_IRQ_RETURN count=%u\n", timer_irq_count );
        }
    } else if( intid == VIRTIO_NET_IRQ_ID ) {
        aicp_virtio_net_isr();
    } else if( unexpected_irq_count < 3U ) {
        unexpected_irq_count++;
        aicp_uart_printf( "AICP_FREERTOS_UNEXPECTED_IRQ count=%u iar=%x intid=%u\n",
                          unexpected_irq_count,
                          iar,
                          intid );
        aicp_uart_printf( "AICP_FREERTOS_UNEXPECTED_TIMER ctl=%lx tval=%lx cval=%lx rpr=%lx\n",
                          ( unsigned long ) read_cntv_ctl(),
                          ( unsigned long ) read_cntv_tval(),
                          ( unsigned long ) read_cntv_cval(),
                          ( unsigned long ) read_icc_rpr() );
    }
}

void aicp_assert_failed( const char * file, uint32_t line )
{
    __asm volatile( "msr daifset, #2" );
    aicp_uart_printf( "AICP_FREERTOS_ASSERT file=%s line=%u\n", file, line );
    for( ;; ) {
        __asm volatile( "wfi" );
    }
}

void vApplicationMallocFailedHook( void )
{
    aicp_assert_failed( "malloc", 0U );
}

void vApplicationIdleHook( void )
{
    /* Let the virtual CPU sleep until the next timer or device interrupt. */
    __asm volatile( "dsb sy; wfi" ::: "memory" );
}

void vApplicationStackOverflowHook( TaskHandle_t task, char * name )
{
    ( void ) task;
    aicp_uart_printf( "AICP_FREERTOS_STACK_OVERFLOW task=%s\n", name );
    aicp_assert_failed( "stack", 0U );
}

void _exit( int status )
{
    ( void ) status;
    for( ;; ) {
        __asm volatile( "wfi" );
    }
}
