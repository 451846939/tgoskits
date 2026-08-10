/* Copyright 2026 The TGOSKits Authors */
/* SPDX-License-Identifier: Apache-2.0 */

#ifndef FREERTOS_CONFIG_H
#define FREERTOS_CONFIG_H

#include <stdint.h>

#define configCPU_CLOCK_HZ                       50000000UL
#define configTICK_RATE_HZ                       100U
#define configUSE_PREEMPTION                     1
#define configUSE_TIME_SLICING                   1
#define configUSE_TICKLESS_IDLE                  0
#define configUSE_IDLE_HOOK                      1
#define configUSE_TICK_HOOK                      0
#define configMAX_PRIORITIES                     10
#define configMINIMAL_STACK_SIZE                 256U
#define configTOTAL_HEAP_SIZE                    ( 2U * 1024U * 1024U )
#define configMAX_TASK_NAME_LEN                  24
#define configUSE_16_BIT_TICKS                   0
#define configIDLE_SHOULD_YIELD                  1
#define configUSE_MUTEXES                        1
#define configUSE_RECURSIVE_MUTEXES              1
#define configUSE_COUNTING_SEMAPHORES            1
#define configQUEUE_REGISTRY_SIZE                8
#define configCHECK_FOR_STACK_OVERFLOW           2
#define configUSE_MALLOC_FAILED_HOOK             1
#define configSUPPORT_STATIC_ALLOCATION          0
#define configSUPPORT_DYNAMIC_ALLOCATION         1
#define configUSE_CO_ROUTINES                    0
#define configUSE_TIMERS                         0
#define configUSE_TRACE_FACILITY                 0
#define configUSE_STATS_FORMATTING_FUNCTIONS     0
#define configGENERATE_RUN_TIME_STATS            0
#define configUSE_NEWLIB_REENTRANT               0
#define configUSE_TASK_NOTIFICATIONS             1
#define configTASK_NOTIFICATION_ARRAY_ENTRIES    1
#define configNUM_THREAD_LOCAL_STORAGE_POINTERS  2
#define configUSE_APPLICATION_TASK_TAG           0
#define configENABLE_BACKWARD_COMPATIBILITY      0
#define configUSE_PORT_OPTIMISED_TASK_SELECTION  1
#define configUSE_TASK_FPU_SUPPORT               2
#define configUNIQUE_INTERRUPT_PRIORITIES        32
#define configMAX_API_CALL_INTERRUPT_PRIORITY    18

#define INCLUDE_vTaskPrioritySet                 1
#define INCLUDE_uxTaskPriorityGet                1
#define INCLUDE_vTaskDelete                      1
#define INCLUDE_vTaskSuspend                     1
#define INCLUDE_vTaskDelayUntil                  1
#define INCLUDE_vTaskDelay                       1
#define INCLUDE_xTaskGetCurrentTaskHandle        1
#define INCLUDE_xTaskGetSchedulerState           1
#define INCLUDE_xTaskGetIdleTaskHandle           1

void vConfigureTickInterrupt( void );
void vClearTickInterrupt( void );
void aicp_assert_failed( const char * file, uint32_t line );

#define configSETUP_TICK_INTERRUPT() vConfigureTickInterrupt()
#define configCLEAR_TICK_INTERRUPT() vClearTickInterrupt()
#define configASSERT( condition )                                      \
    do {                                                               \
        if( !( condition ) ) {                                         \
            aicp_assert_failed( __FILE__, ( uint32_t ) __LINE__ );      \
        }                                                              \
    } while( 0 )

#endif
