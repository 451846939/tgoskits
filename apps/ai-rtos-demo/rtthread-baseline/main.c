// Copyright 2026 The TGOSKits Authors
//
// SPDX-License-Identifier: Apache-2.0

#include <rtthread.h>

#include "gtimer.h"

#define PERIOD_MS 20U
#define SAMPLE_COUNT 1000U
#define STRESS_STACK_SIZE 4096U
#define STRESS_PRIORITY 20U

static struct rt_semaphore period_sem;
static struct rt_timer period_timer;

#if defined(AICP_BASELINE_STRESS)
static rt_uint8_t stress_stack_0[STRESS_STACK_SIZE];
static rt_uint8_t stress_stack_1[STRESS_STACK_SIZE];
static struct rt_thread stress_thread_0;
static struct rt_thread stress_thread_1;

static void stress_worker(void *argument)
{
    volatile rt_uint64_t accumulator = (rt_ubase_t)argument + 1U;

    for (;;)
    {
        for (rt_uint32_t i = 0; i < 200000U; ++i)
        {
            accumulator = accumulator * 6364136223846793005ULL + 1ULL;
        }
        rt_thread_mdelay(1);
    }
}
#endif

static rt_uint64_t cycles_to_ns(rt_uint64_t cycles, rt_uint64_t frequency)
{
    return (cycles / frequency) * 1000000000ULL +
           (cycles % frequency) * 1000000000ULL / frequency;
}

static rt_uint64_t abs_diff_u64(rt_uint64_t lhs, rt_uint64_t rhs)
{
    return lhs >= rhs ? lhs - rhs : rhs - lhs;
}

static void sort_u64(rt_uint64_t *values, rt_size_t count)
{
    for (rt_size_t i = 1; i < count; ++i)
    {
        const rt_uint64_t value = values[i];
        rt_size_t j = i;

        while (j > 0 && values[j - 1] > value)
        {
            values[j] = values[j - 1];
            --j;
        }
        values[j] = value;
    }
}

static void period_expiry(void *parameter)
{
    RT_UNUSED(parameter);
    rt_sem_release(&period_sem);
}

int main(void)
{
    static rt_uint64_t abs_jitter_ns[SAMPLE_COUNT];
    static rt_uint64_t interval_jitter_ns[SAMPLE_COUNT];
    const rt_uint64_t period_ns = PERIOD_MS * 1000000ULL;
    const rt_uint64_t frequency = rt_hw_get_gtimer_frq();
    const rt_uint64_t period_cycles = frequency * PERIOD_MS / 1000ULL;
    rt_uint64_t expected_cycles;
    rt_uint64_t previous_cycles;
    rt_uint64_t jitter_sum = 0;
    rt_uint64_t interval_jitter_sum = 0;
    rt_uint64_t max_jitter = 0;
    rt_uint64_t max_interval_jitter = 0;
    rt_uint32_t missed_deadlines = 0;

#if defined(AICP_BASELINE_STRESS)
    rt_thread_init(&stress_thread_0, "stress0", stress_worker, (void *)0,
                   stress_stack_0, sizeof(stress_stack_0), STRESS_PRIORITY, 10);
    rt_thread_init(&stress_thread_1, "stress1", stress_worker, (void *)1,
                   stress_stack_1, sizeof(stress_stack_1), STRESS_PRIORITY, 10);
    rt_thread_startup(&stress_thread_0);
    rt_thread_startup(&stress_thread_1);
#endif

    rt_kprintf("AICP_RTTHREAD_BASELINE_START mode=%s samples=%u period_ns=%llu "
               "timer_freq_hz=%llu stress_workers=%u\n",
#if defined(AICP_BASELINE_STRESS)
               "stress",
#else
               "idle",
#endif
               SAMPLE_COUNT, period_ns, frequency,
#if defined(AICP_BASELINE_STRESS)
               2U
#else
               0U
#endif
    );

    rt_sem_init(&period_sem, "period", 0, RT_IPC_FLAG_PRIO);
    rt_timer_init(&period_timer, "period", period_expiry, RT_NULL,
                  rt_tick_from_millisecond(PERIOD_MS),
                  RT_TIMER_FLAG_PERIODIC | RT_TIMER_FLAG_HARD_TIMER);

    previous_cycles = rt_hw_get_cntpct_val();
    expected_cycles = previous_cycles + period_cycles;
    rt_timer_start(&period_timer);

    for (rt_size_t i = 0; i < SAMPLE_COUNT; ++i)
    {
        rt_uint64_t now_cycles;
        rt_uint64_t jitter_cycles;
        rt_uint64_t jitter_ns;
        rt_uint64_t interval_cycles;
        rt_uint64_t interval_error_cycles;
        rt_uint64_t interval_error_ns;

        rt_sem_take(&period_sem, RT_WAITING_FOREVER);
        now_cycles = rt_hw_get_cntpct_val();
        jitter_cycles = abs_diff_u64(now_cycles, expected_cycles);
        jitter_ns = cycles_to_ns(jitter_cycles, frequency);
        interval_cycles = now_cycles - previous_cycles;
        interval_error_cycles = abs_diff_u64(interval_cycles, period_cycles);
        interval_error_ns = cycles_to_ns(interval_error_cycles, frequency);
        abs_jitter_ns[i] = jitter_ns;
        interval_jitter_ns[i] = interval_error_ns;
        jitter_sum += jitter_ns;
        interval_jitter_sum += interval_error_ns;
        if (jitter_ns > max_jitter)
        {
            max_jitter = jitter_ns;
        }
        if (interval_error_ns > max_interval_jitter)
        {
            max_interval_jitter = interval_error_ns;
        }
        if (now_cycles > expected_cycles + period_cycles)
        {
            ++missed_deadlines;
        }
        previous_cycles = now_cycles;
        expected_cycles += period_cycles;
    }

    rt_timer_stop(&period_timer);
    sort_u64(abs_jitter_ns, SAMPLE_COUNT);
    sort_u64(interval_jitter_ns, SAMPLE_COUNT);

    /* Keep each marker short: secondary CPUs can otherwise interleave their
     * startup output with a long UART line under QEMU SMP. */
    rt_kprintf("AICP_RTTHREAD_RESULT mode=%s key=samples value=%u\n",
#if defined(AICP_BASELINE_STRESS)
               "stress",
#else
               "idle",
#endif
               SAMPLE_COUNT);
#if defined(AICP_BASELINE_STRESS)
#define AICP_RESULT_MODE "stress"
#else
#define AICP_RESULT_MODE "idle"
#endif
    rt_kprintf("AICP_RTTHREAD_RESULT mode=%s key=period_ns value=%llu\n",
               AICP_RESULT_MODE, period_ns);
    rt_kprintf("AICP_RTTHREAD_RESULT mode=%s key=avg_abs_jitter_ns value=%llu\n",
               AICP_RESULT_MODE, jitter_sum / SAMPLE_COUNT);
    rt_kprintf("AICP_RTTHREAD_RESULT mode=%s key=p99_abs_jitter_ns value=%llu\n",
               AICP_RESULT_MODE,
               abs_jitter_ns[((SAMPLE_COUNT * 99U) + 99U) / 100U - 1U]);
    rt_kprintf("AICP_RTTHREAD_RESULT mode=%s key=max_abs_jitter_ns value=%llu\n",
               AICP_RESULT_MODE, max_jitter);
    rt_kprintf("AICP_RTTHREAD_RESULT mode=%s key=missed_deadlines value=%u\n",
               AICP_RESULT_MODE, missed_deadlines);
    rt_kprintf("AICP_RTTHREAD_RESULT mode=%s key=avg_interval_jitter_ns value=%llu\n",
               AICP_RESULT_MODE, interval_jitter_sum / SAMPLE_COUNT);
    rt_kprintf("AICP_RTTHREAD_RESULT mode=%s key=p99_interval_jitter_ns value=%llu\n",
               AICP_RESULT_MODE,
               interval_jitter_ns[((SAMPLE_COUNT * 99U) + 99U) / 100U - 1U]);
    rt_kprintf("AICP_RTTHREAD_RESULT mode=%s key=max_interval_jitter_ns value=%llu\n",
               AICP_RESULT_MODE, max_interval_jitter);
    rt_kprintf("AICP_RTTHREAD_BASELINE_DONE mode=%s\n", AICP_RESULT_MODE);
#undef AICP_RESULT_MODE

    return 0;
}
