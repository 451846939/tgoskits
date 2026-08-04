/*
 * SPDX-License-Identifier: Apache-2.0
 *
 * AxVisor IVC publisher demo for Zephyr.
 */

#include <stdint.h>
#include <stddef.h>
#include <string.h>

#include <zephyr/kernel.h>
#include <zephyr/kernel/internal/mm.h>
#include <zephyr/sys/printk.h>
#include <zephyr/sys/util.h>

#if defined(CONFIG_MMU)
#include <zephyr/arch/arm64/arm_mem.h>
#include <zephyr/sys/device_mmio.h>
#endif

#define AXIVC_CHANNEL_KEY 0x49564301ULL
#define AXIVC_CHANNEL_SIZE 0x10000ULL
#define AXIVC_PUBLISHER_VM_ID 1ULL
#define AXIVC_SUBSCRIBER_VM_ID 2ULL

#define AXIVC_REGION_MAGIC 0x49564332U
#define AXIVC_REGION_VERSION 2U
#define AXIVC_REGION_FEATURE_SPSC_FIXED_SLOTS 1U
#define AXIVC_SLOT_PAYLOAD_SIZE 48U
#define AXIVC_RING_CAPACITY 16U
#define AXIVC_MESSAGE_KIND_REQUEST 1U
#define AXIVC_RING_PUBLISHER_TO_SUBSCRIBER 1U
#define AXIVC_RING_SUBSCRIBER_TO_PUBLISHER 2U

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

struct axivc_region {
	uint64_t publisher_id;
	uint64_t key;
	struct axivc_region_header header;
	struct axivc_ring publisher_to_subscriber;
	struct axivc_ring subscriber_to_publisher;
} __aligned(64);

BUILD_ASSERT(sizeof(struct axivc_region_header) == 32);
BUILD_ASSERT(sizeof(struct axivc_message_slot) == 64);
BUILD_ASSERT(sizeof(struct axivc_ring) == 1088);
BUILD_ASSERT(offsetof(struct axivc_region, publisher_to_subscriber) == 64);
BUILD_ASSERT(offsetof(struct axivc_region, subscriber_to_publisher) == 1152);
BUILD_ASSERT(sizeof(struct axivc_region) == 2240);

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
#if defined(CONFIG_MMU)
	uint8_t *mapped = NULL;
	k_mem_map_phys_bare(&mapped, (uintptr_t)gpa, (size_t)size,
			    K_MEM_ARM_NORMAL_NC | K_MEM_PERM_RW);
	return mapped;
#else
	ARG_UNUSED(size);
	return (void *)(uintptr_t)gpa;
#endif
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
	__atomic_store_n(&region->header.version, AXIVC_REGION_VERSION,
			 __ATOMIC_RELEASE);
	__atomic_store_n(&region->header.magic, AXIVC_REGION_MAGIC,
			 __ATOMIC_RELEASE);
}

static int ring_send(struct axivc_ring *ring, uint32_t kind, uint64_t sequence,
		     const uint8_t *payload, size_t payload_len)
{
	uint32_t tail = __atomic_load_n(&ring->tail, __ATOMIC_RELAXED);
	uint32_t head = __atomic_load_n(&ring->head, __ATOMIC_ACQUIRE);
	struct axivc_message_slot *slot;
	size_t len;

	if ((uint32_t)(tail - head) >= AXIVC_RING_CAPACITY) {
		return -1;
	}

	slot = &ring->slots[tail % AXIVC_RING_CAPACITY];
	len = MIN(payload_len, (size_t)AXIVC_SLOT_PAYLOAD_SIZE);
	memcpy(slot->payload, payload, len);
	if (len < AXIVC_SLOT_PAYLOAD_SIZE) {
		memset(slot->payload + len, 0, AXIVC_SLOT_PAYLOAD_SIZE - len);
	}
	__atomic_store_n(&slot->sequence, sequence, __ATOMIC_RELAXED);
	__atomic_store_n(&slot->len, (uint32_t)len, __ATOMIC_RELAXED);
	__atomic_store_n(&slot->kind, kind, __ATOMIC_RELAXED);
	__atomic_store_n(&ring->tail, tail + 1, __ATOMIC_RELEASE);
	return 0;
}

static void notify_linux(void)
{
	uint64_t ret = hvc_call(HIVC_NOTIFY, AXIVC_PUBLISHER_VM_ID,
				AXIVC_CHANNEL_KEY, AXIVC_SUBSCRIBER_VM_ID, 0, 0,
				0);
	if (ret != 0) {
		printk("zephyr ivc notify warning ret=%llu\n",
		       (unsigned long long)ret);
	}
}

int main(void)
{
	uint64_t shm_base = 0;
	uint64_t shm_size = AXIVC_CHANNEL_SIZE;
	struct axivc_region *region;
	uint64_t ret;

	printk("zephyr ivc publisher start\n");

	ret = hvc_call(HIVC_PUBLISH_CHANNEL, AXIVC_CHANNEL_KEY,
		       guest_phys_addr(&shm_base), guest_phys_addr(&shm_size), 0,
		       0, 0);
	if (ret != 0) {
		printk("zephyr ivc publish failed ret=%llu\n",
		       (unsigned long long)ret);
		return 1;
	}
	if (shm_size < sizeof(struct axivc_region)) {
		printk("zephyr ivc publish failed size=%llu need=%zu\n",
		       (unsigned long long)shm_size, sizeof(struct axivc_region));
		return 1;
	}

	printk("zephyr ivc publish ok base=0x%llx size=%llu\n", shm_base,
	       (unsigned long long)shm_size);
	region = map_shared_region(shm_base, shm_size);
	if (region == NULL) {
		printk("zephyr ivc map failed base=0x%llx\n",
		       (unsigned long long)shm_base);
		return 1;
	}
	region_init(region);

	for (uint64_t seq = 1; seq <= 5; seq++) {
		static const uint8_t msg[] = "hello from zephyr publisher";

		while (ring_send(&region->publisher_to_subscriber,
				 AXIVC_MESSAGE_KIND_REQUEST, seq, msg,
				 sizeof(msg) - 1) != 0) {
			k_busy_wait(1000);
		}
		printk("zephyr ivc send seq=%llu\n", (unsigned long long)seq);
		notify_linux();
		k_busy_wait(100000);
	}

	printk("zephyr ivc publisher sent 5 messages\n");
	return 0;
}
