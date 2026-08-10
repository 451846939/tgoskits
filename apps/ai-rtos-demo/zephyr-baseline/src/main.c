// Copyright 2026 The TGOSKits Authors
//
// SPDX-License-Identifier: Apache-2.0

#include <stdint.h>
#include <zephyr/kernel.h>
#include <zephyr/sys/printk.h>

#define PERIOD_MS 20
#define SAMPLE_COUNT 1000
#define STRESS_STACK_SIZE 2048
#define STRESS_PRIORITY 7

K_SEM_DEFINE(period_sem, 0, 1);
K_TIMER_DEFINE(period_timer, NULL, NULL);

#if defined(CONFIG_AICP_BASELINE_STRESS)
K_THREAD_STACK_DEFINE(stress_stack_0, STRESS_STACK_SIZE);
K_THREAD_STACK_DEFINE(stress_stack_1, STRESS_STACK_SIZE);
static struct k_thread stress_thread_0;
static struct k_thread stress_thread_1;

static void stress_worker(void *arg0, void *arg1, void *arg2)
{
	ARG_UNUSED(arg0);
	ARG_UNUSED(arg1);
	ARG_UNUSED(arg2);

	for (;;) {
		k_busy_wait(3000);
		k_sleep(K_MSEC(1));
	}
}
#endif

static uint64_t abs_diff_u64(uint64_t lhs, uint64_t rhs)
{
	return lhs >= rhs ? lhs - rhs : rhs - lhs;
}

static uint64_t monotonic_ns(void)
{
	return k_cyc_to_ns_floor64(k_cycle_get_64());
}

static void sort_u64(uint64_t *values, size_t count)
{
	for (size_t i = 1; i < count; ++i) {
		uint64_t value = values[i];
		size_t j = i;

		while (j > 0 && values[j - 1] > value) {
			values[j] = values[j - 1];
			--j;
		}
		values[j] = value;
	}
}

static void period_expiry(struct k_timer *timer)
{
	ARG_UNUSED(timer);
	k_sem_give(&period_sem);
}

int main(void)
{
	static uint64_t abs_jitter_ns[SAMPLE_COUNT];
	const uint64_t period_ns = PERIOD_MS * 1000000ULL;
	uint64_t expected_ns;
	uint64_t jitter_sum = 0;
	uint64_t max_jitter = 0;
	uint32_t missed_deadlines = 0;

#if defined(CONFIG_AICP_BASELINE_STRESS)
	k_thread_create(&stress_thread_0, stress_stack_0, STRESS_STACK_SIZE,
			stress_worker, NULL, NULL, NULL, STRESS_PRIORITY, 0, K_NO_WAIT);
	k_thread_create(&stress_thread_1, stress_stack_1, STRESS_STACK_SIZE,
			stress_worker, NULL, NULL, NULL, STRESS_PRIORITY, 0, K_NO_WAIT);
#endif

	printk("AICP_ZEPHYR_BASELINE_START mode=%s samples=%d period_ns=%llu\n",
#if defined(CONFIG_AICP_BASELINE_STRESS)
	       "stress",
#else
	       "idle",
#endif
	       SAMPLE_COUNT, period_ns);

	expected_ns = monotonic_ns() + period_ns;
	k_timer_init(&period_timer, period_expiry, NULL);
	k_timer_start(&period_timer, K_MSEC(PERIOD_MS), K_MSEC(PERIOD_MS));

	for (size_t i = 0; i < SAMPLE_COUNT; ++i) {
		k_sem_take(&period_sem, K_FOREVER);
		uint64_t now_ns = monotonic_ns();
		uint64_t jitter_ns = abs_diff_u64(now_ns, expected_ns);

		abs_jitter_ns[i] = jitter_ns;
		jitter_sum += jitter_ns;
		if (jitter_ns > max_jitter) {
			max_jitter = jitter_ns;
		}
		if (now_ns > expected_ns + period_ns) {
			++missed_deadlines;
		}
		expected_ns += period_ns;
	}

	k_timer_stop(&period_timer);
	sort_u64(abs_jitter_ns, SAMPLE_COUNT);
	const size_t p99_index = ((SAMPLE_COUNT * 99U) + 99U) / 100U - 1U;

	printk("AICP_ZEPHYR_BASELINE_DONE mode=%s samples=%d period_ns=%llu "
	       "avg_abs_jitter_ns=%llu p99_abs_jitter_ns=%llu "
	       "max_abs_jitter_ns=%llu missed_deadlines=%u\n",
#if defined(CONFIG_AICP_BASELINE_STRESS)
	       "stress",
#else
	       "idle",
#endif
	       SAMPLE_COUNT, period_ns, jitter_sum / SAMPLE_COUNT,
	       abs_jitter_ns[p99_index], max_jitter, missed_deadlines);

	return 0;
}
