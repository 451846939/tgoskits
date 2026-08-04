/*
 * SPDX-License-Identifier: Apache-2.0
 *
 * AxVisor IVC shared-memory throughput publisher for Zephyr.
 */

#include <stdint.h>
#include <stddef.h>
#include <string.h>

#include <zephyr/kernel.h>
#include <zephyr/kernel/internal/mm.h>
#include <zephyr/kernel/mm.h>
#include <zephyr/arch/arm64/arm_mem.h>
#include <zephyr/sys/printk.h>
#include <zephyr/sys/util.h>

#define AXIVC_CHANNEL_KEY 0x49564301ULL
#define AXIVC_CHANNEL_SIZE 0x1000000ULL
#define AXIVC_PUBLISHER_VM_ID 1ULL
#define AXIVC_SUBSCRIBER_VM_ID 2ULL

#define AXIVC_REGION_MAGIC 0x49564332U
#define AXIVC_REGION_VERSION 2U
#define AXIVC_REGION_FEATURE_SPSC_FIXED_SLOTS 1U
#define AXIVC_SLOT_PAYLOAD_SIZE 48U
#define AXIVC_RING_CAPACITY 16U
#define AXIVC_RING_PUBLISHER_TO_SUBSCRIBER 1U
#define AXIVC_RING_SUBSCRIBER_TO_PUBLISHER 2U

#define AXIVC_PERF_MAGIC 0x49565046U
#define AXIVC_PERF_VERSION 1U
#define AXIVC_PERF_ITERATIONS 100U
#define AXIVC_PERF_TEST_COUNT 4U
#define AXIVC_PERF_STATE_IDLE 0U
#define AXIVC_PERF_STATE_READY 1U
#define AXIVC_PERF_STATE_DONE 2U
#define AXIVC_PERF_STATE_COMPLETE 3U

#define HIVC_PUBLISH_CHANNEL 3U
#define HIVC_NOTIFY 7U

struct axivc_region_header {
	uint32_t magic;
	uint32_t version;
	uint32_t header_size;
	uint32_t region_size;
	uint32_t features;
	uint32_t publisher_to_subscriber_offset;
	uint32_t subscriber_to_publisher_offset;
	uint32_t ring_size;
} __aligned(8);

struct axivc_message_slot {
	uint64_t sequence;
	uint32_t len;
	uint32_t kind;
	uint8_t payload[AXIVC_SLOT_PAYLOAD_SIZE];
} __aligned(64);

struct axivc_ring {
	uint32_t direction;
	uint32_t capacity;
	uint32_t slot_payload_size;
	uint32_t head;
	uint32_t tail;
	uint32_t reserved[3];
	struct axivc_message_slot slots[AXIVC_RING_CAPACITY];
} __aligned(64);

struct axivc_perf_control {
	uint32_t magic;
	uint32_t version;
	uint32_t state;
	uint32_t test_index;
	uint32_t iteration;
	uint32_t reserved0;
	uint64_t bytes;
	uint64_t checksum;
	uint64_t write_ns;
} __aligned(64);

struct axivc_region {
	uint64_t publisher_id;
	uint64_t key;
	struct axivc_region_header header;
	struct axivc_ring publisher_to_subscriber;
	struct axivc_ring subscriber_to_publisher;
	struct axivc_perf_control perf;
} __aligned(64);

BUILD_ASSERT(sizeof(struct axivc_region_header) == 32);
BUILD_ASSERT(sizeof(struct axivc_message_slot) == 64);
BUILD_ASSERT(sizeof(struct axivc_ring) == 1088);
BUILD_ASSERT(offsetof(struct axivc_region, publisher_to_subscriber) == 64);
BUILD_ASSERT(offsetof(struct axivc_region, subscriber_to_publisher) == 1152);
BUILD_ASSERT(offsetof(struct axivc_region, perf) == 2240);

static const uint32_t perf_sizes[AXIVC_PERF_TEST_COUNT] = {
	256U * 1024U,
	512U * 1024U,
	1024U * 1024U,
	10U * 1024U * 1024U,
};

static uint64_t hvc_call(uint64_t code, uint64_t arg0, uint64_t arg1,
			 uint64_t arg2, uint64_t arg3, uint64_t arg4,
			 uint64_t arg5)
{
	register uint64_t x0 __asm__("x0") = code;
	register uint64_t x1 __asm__("x1") = arg0;
	register uint64_t x2 __asm__("x2") = arg1;
	register uint64_t x3 __asm__("x3") = arg2;
	register uint64_t x4 __asm__("x4") = arg3;
	register uint64_t x5 __asm__("x5") = arg4;
	register uint64_t x6 __asm__("x6") = arg5;

	__asm__ volatile("hvc #0"
			 : "+r"(x0)
			 : "r"(x1), "r"(x2), "r"(x3), "r"(x4), "r"(x5),
			   "r"(x6)
			 : "memory");

	return x0;
}

static uintptr_t guest_phys_addr(void *ptr)
{
#if defined(CONFIG_MMU)
	return k_mem_phys_addr(ptr);
#else
	return (uintptr_t)ptr;
#endif
}

static void *map_shared_region(uint64_t gpa, uint64_t size)
{
	uint8_t *mapped = NULL;

	k_mem_map_phys_bare(&mapped, (uintptr_t)gpa, (size_t)size,
			    K_MEM_DIRECT_MAP | K_MEM_ARM_NORMAL_NC | K_MEM_PERM_RW);
	return mapped;
}

static void ring_init(struct axivc_ring *ring, uint32_t direction)
{
	memset(ring, 0, sizeof(*ring));
	__atomic_store_n(&ring->direction, direction, __ATOMIC_RELAXED);
	__atomic_store_n(&ring->capacity, AXIVC_RING_CAPACITY, __ATOMIC_RELAXED);
	__atomic_store_n(&ring->slot_payload_size, AXIVC_SLOT_PAYLOAD_SIZE,
			 __ATOMIC_RELAXED);
	__atomic_store_n(&ring->head, 0, __ATOMIC_RELAXED);
	__atomic_store_n(&ring->tail, 0, __ATOMIC_RELEASE);
}

static void region_init(struct axivc_region *region)
{
	memset(region, 0, sizeof(*region));
	region->publisher_id = AXIVC_PUBLISHER_VM_ID;
	region->key = AXIVC_CHANNEL_KEY;
	ring_init(&region->publisher_to_subscriber,
		  AXIVC_RING_PUBLISHER_TO_SUBSCRIBER);
	ring_init(&region->subscriber_to_publisher,
		  AXIVC_RING_SUBSCRIBER_TO_PUBLISHER);

	__atomic_store_n(&region->header.header_size,
			 sizeof(struct axivc_region_header), __ATOMIC_RELAXED);
	__atomic_store_n(&region->header.region_size, sizeof(struct axivc_region),
			 __ATOMIC_RELAXED);
	__atomic_store_n(&region->header.features,
			 AXIVC_REGION_FEATURE_SPSC_FIXED_SLOTS, __ATOMIC_RELAXED);
	__atomic_store_n(&region->header.publisher_to_subscriber_offset,
			 offsetof(struct axivc_region, publisher_to_subscriber),
			 __ATOMIC_RELAXED);
	__atomic_store_n(&region->header.subscriber_to_publisher_offset,
			 offsetof(struct axivc_region, subscriber_to_publisher),
			 __ATOMIC_RELAXED);
	__atomic_store_n(&region->header.ring_size, sizeof(struct axivc_ring),
			 __ATOMIC_RELAXED);
	__atomic_store_n(&region->perf.magic, AXIVC_PERF_MAGIC,
			 __ATOMIC_RELAXED);
	__atomic_store_n(&region->perf.version, AXIVC_PERF_VERSION,
			 __ATOMIC_RELAXED);
	__atomic_store_n(&region->perf.state, AXIVC_PERF_STATE_IDLE,
			 __ATOMIC_RELEASE);
	__atomic_store_n(&region->header.version, AXIVC_REGION_VERSION,
			 __ATOMIC_RELEASE);
	__atomic_store_n(&region->header.magic, AXIVC_REGION_MAGIC,
			 __ATOMIC_RELEASE);
}

static void notify_linux(void)
{
	(void)hvc_call(HIVC_NOTIFY, AXIVC_PUBLISHER_VM_ID, AXIVC_CHANNEL_KEY,
		       AXIVC_SUBSCRIBER_VM_ID, 0, 0, 0);
}

static uint8_t prng_next(uint32_t *state)
{
	*state = (*state * 1664525U) + 1013904223U;
	return (uint8_t)(*state >> 24);
}

static uint64_t fill_random(uint8_t *buf, size_t len, uint32_t seed)
{
	uint64_t checksum = 0;
	uint32_t state = seed;

	for (size_t i = 0; i < len; i++) {
		uint8_t value = prng_next(&state);
		buf[i] = value;
		checksum += value;
	}
	return checksum;
}

static void wait_for_state(struct axivc_perf_control *perf, uint32_t state)
{
	while (__atomic_load_n(&perf->state, __ATOMIC_ACQUIRE) != state) {
		k_busy_wait(50);
	}
}

int main(void)
{
	uint64_t shm_base = 0;
	uint64_t shm_size = AXIVC_CHANNEL_SIZE;
	struct axivc_region *region;
	struct axivc_perf_control *perf;
	uint8_t *payload;
	size_t payload_capacity;
	uint64_t ret;

	printk("zephyr ivc perf publisher start\n");

	ret = hvc_call(HIVC_PUBLISH_CHANNEL, AXIVC_CHANNEL_KEY,
		       guest_phys_addr(&shm_base), guest_phys_addr(&shm_size), 0,
		       0, 0);
	if (ret != 0) {
		printk("zephyr ivc publish failed ret=%llu\n",
		       (unsigned long long)ret);
		return 1;
	}
	if (shm_size < AXIVC_CHANNEL_SIZE) {
		printk("zephyr ivc publish failed size=%llu need=%llu\n",
		       (unsigned long long)shm_size,
		       (unsigned long long)AXIVC_CHANNEL_SIZE);
		return 1;
	}

	region = map_shared_region(shm_base, shm_size);
	if (region == NULL) {
		printk("zephyr ivc map failed base=0x%llx\n",
		       (unsigned long long)shm_base);
		return 1;
	}
	region_init(region);

	perf = &region->perf;
	payload = (uint8_t *)region + ROUND_UP(sizeof(struct axivc_region), 64);
	payload_capacity = (uint8_t *)region + shm_size - payload;

	printk("zephyr ivc perf shared base=0x%llx size=%llu payload=%zu\n",
	       (unsigned long long)shm_base, (unsigned long long)shm_size,
	       payload_capacity);

	for (uint32_t test = 0; test < AXIVC_PERF_TEST_COUNT; test++) {
		uint64_t total_ns = 0;
		size_t bytes = perf_sizes[test];

		if (bytes > payload_capacity) {
			printk("zephyr ivc perf failed size=%zu capacity=%zu\n",
			       bytes, payload_capacity);
			return 1;
		}

		for (uint32_t iter = 0; iter < AXIVC_PERF_ITERATIONS; iter++) {
			uint64_t checksum;
			uint64_t start_ns;
			uint64_t end_ns;
			uint32_t seed = 0x49564300U ^ (test << 16) ^ iter;

			wait_for_state(perf, AXIVC_PERF_STATE_IDLE);
			start_ns = k_cycle_get_64() * 1000000000ULL /
				   sys_clock_hw_cycles_per_sec();
			checksum = fill_random(payload, bytes, seed);
			end_ns = k_cycle_get_64() * 1000000000ULL /
				 sys_clock_hw_cycles_per_sec();
			total_ns += end_ns - start_ns;

			__atomic_store_n(&perf->test_index, test,
					 __ATOMIC_RELAXED);
			__atomic_store_n(&perf->iteration, iter,
					 __ATOMIC_RELAXED);
			__atomic_store_n(&perf->bytes, bytes, __ATOMIC_RELAXED);
			__atomic_store_n(&perf->checksum, checksum,
					 __ATOMIC_RELAXED);
			__atomic_store_n(&perf->write_ns, end_ns - start_ns,
					 __ATOMIC_RELAXED);
			__atomic_store_n(&perf->state, AXIVC_PERF_STATE_READY,
					 __ATOMIC_RELEASE);
			notify_linux();
			wait_for_state(perf, AXIVC_PERF_STATE_DONE);
			__atomic_store_n(&perf->state, AXIVC_PERF_STATE_IDLE,
					 __ATOMIC_RELEASE);
		}

		printk("zephyr ivc write size=%zu iterations=%u avg=%llu B/s\n",
		       bytes, AXIVC_PERF_ITERATIONS,
		       (unsigned long long)((uint64_t)bytes *
					    AXIVC_PERF_ITERATIONS *
					    1000000000ULL / total_ns));
	}

	wait_for_state(perf, AXIVC_PERF_STATE_IDLE);
	__atomic_store_n(&perf->state, AXIVC_PERF_STATE_COMPLETE,
			 __ATOMIC_RELEASE);
	notify_linux();
	printk("zephyr ivc perf publisher pass\n");
	return 0;
}
