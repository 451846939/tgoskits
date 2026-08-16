/*
 * Copyright 2026 The TGOSKits Authors
 *
 * SPDX-License-Identifier: Apache-2.0
 */

/*
 * Legacy virtio-mmio transport for the AxVisor/QEMU guest profile.
 *
 * Zephyr v4.4's upstream virtio-mmio transport supports only version 2
 * (modern) devices.  AxVisor's verified nested virtio path uses QEMU's
 * version 1 legacy transport, whose queues are selected through QUEUE_PFN and
 * whose used ring must be page aligned.  This application-owned transport
 * keeps the upstream virtio-net driver and generic virtqueue operations, but
 * provides the legacy MMIO setup and legacy vring allocation rules.
 */

#include <errno.h>
#include <string.h>
#include <zephyr/device.h>
#include <zephyr/drivers/virtio.h>
#include <zephyr/drivers/virtio/virtio_config.h>
#include <zephyr/drivers/virtio/virtqueue.h>
#include <zephyr/kernel.h>
#include <zephyr/kernel/mm.h>
#include <zephyr/logging/log.h>
#include <zephyr/spinlock.h>
#include <zephyr/sys/atomic.h>
#include <zephyr/sys/barrier.h>
#include <zephyr/sys/byteorder.h>
#include <zephyr/sys/sys_io.h>
#include <zephyr/sys/util.h>

#define DT_DRV_COMPAT virtio_mmio

LOG_MODULE_REGISTER(aicp_virtio_mmio_legacy, CONFIG_VIRTIO_LOG_LEVEL);

#define AICP_VIRTIO_MMIO_MAGIC              0x74726976u
#define AICP_VIRTIO_MMIO_LEGACY_VERSION     1u
#define AICP_VIRTIO_MMIO_INVALID_DEVICE_ID  0u
#define AICP_VIRTIO_MMIO_GUEST_PAGE_SIZE    0x028u
#define AICP_VIRTIO_MMIO_QUEUE_ALIGN        0x03cu
#define AICP_VIRTIO_MMIO_QUEUE_PFN          0x040u
#define AICP_VIRTIO_LEGACY_PAGE_SIZE        4096u
#define AICP_VIRTIO_LEGACY_MIN_QUEUE_SIZE   8u
#define AICP_VIRTIO_POLL_INTERVAL           K_MSEC(1)
#define AICP_VIRTIO_COMPLETION_STACK_SIZE   2048u
#define AICP_VIRTIO_COMPLETION_PRIORITY     K_LOWEST_APPLICATION_THREAD_PRIO
#define AICP_VIRTIO_TRACE_LIMIT             8u
#define AICP_VIRTIO_NET_F_MRG_RXBUF         15
#define AICP_VIRTIO_F_ANY_LAYOUT            27

#define DEV_CFG(dev)  ((const struct aicp_virtio_mmio_config *)(dev)->config)
#define DEV_DATA(dev) ((struct aicp_virtio_mmio_data *)(dev)->data)

struct aicp_virtio_mmio_data {
	DEVICE_MMIO_NAMED_RAM(reg_base);

	struct virtq *virtqueues;
	void **queue_areas;
	uint16_t virtqueue_count;
	uint32_t driver_features;
	const struct device *dev;
	struct k_sem completion_sem;
	struct k_thread completion_thread;
	K_KERNEL_STACK_MEMBER(completion_stack,
			      AICP_VIRTIO_COMPLETION_STACK_SIZE);
	atomic_t pending_isr_status;
	uint32_t irq_count;
	uint32_t poll_count;
	uint32_t poll_recovery_count;
	uint32_t notify_count;

	struct k_spinlock isr_lock;
	struct k_spinlock notify_lock;
};

struct aicp_virtio_mmio_config {
	DEVICE_MMIO_NAMED_ROM(reg_base);
};

/* Provided by Zephyr's generic virtio_common.c when CONFIG_VIRTIO=y. */
extern void virtio_isr(const struct device *dev, uint8_t isr_status,
		       uint16_t virtqueue_count);

static inline uint32_t legacy_read32(const struct device *dev, uint32_t offset)
{
	const mem_addr_t reg = DEVICE_MMIO_NAMED_GET(dev, reg_base) + offset;

	barrier_dmem_fence_full();
	return sys_le32_to_cpu(sys_read32(reg));
}

static inline void legacy_write32(const struct device *dev, uint32_t offset,
				  uint32_t value)
{
	const mem_addr_t reg = DEVICE_MMIO_NAMED_GET(dev, reg_base) + offset;

	sys_write32(sys_cpu_to_le32(value), reg);
	barrier_dmem_fence_full();
}

static void legacy_process_queues(struct aicp_virtio_mmio_data *data)
{
	uint32_t status;

	do {
		status = (uint32_t)atomic_set(&data->pending_isr_status, 0);
		if (status != 0u) {
			virtio_isr(data->dev, (uint8_t)status,
				   data->virtqueue_count);
		}
	} while (atomic_get(&data->pending_isr_status) != 0);
}

static bool legacy_queues_have_used(struct aicp_virtio_mmio_data *data)
{
	bool pending = false;

	barrier_dmem_fence_full();
	for (uint16_t i = 0; i < data->virtqueue_count; ++i) {
		const struct virtq *vq = &data->virtqueues[i];
		const uint16_t used_idx = sys_le16_to_cpu(vq->used->idx);

		if (vq->last_used_idx != used_idx) {
			pending = true;
		}
	}
	return pending;
}

static void legacy_completion_thread(void *p1, void *p2, void *p3)
{
	struct aicp_virtio_mmio_data *data = p1;

	ARG_UNUSED(p2);
	ARG_UNUSED(p3);
	LOG_INF("legacy completion thread ready priority=%d poll_ms=1",
		AICP_VIRTIO_COMPLETION_PRIORITY);
	for (;;) {
		const bool irq_signaled =
			k_sem_take(&data->completion_sem, K_NO_WAIT) == 0;
		bool used_pending;

		++data->poll_count;
		used_pending = legacy_queues_have_used(data);
		if (used_pending && !irq_signaled &&
		    atomic_get(&data->pending_isr_status) == 0) {
			++data->poll_recovery_count;
			if (data->poll_recovery_count <= AICP_VIRTIO_TRACE_LIMIT) {
				LOG_WRN("legacy poll recovered used buffers count=%u",
					data->poll_recovery_count);
			}
			atomic_or(&data->pending_isr_status,
				  VIRTIO_QUEUE_INTERRUPT);
		}
		if (irq_signaled || used_pending ||
		    atomic_get(&data->pending_isr_status) != 0) {
			legacy_process_queues(data);
		}
		/*
		 * AxVisor's current GPPT GICR path boots Zephyr but does not deliver
		 * the architectural timer PPI reliably. Use the counter-based busy
		 * wait only in this lowest-priority fallback thread, then yield so
		 * network and control work always take precedence.
		 */
		k_busy_wait(1000u);
		k_yield();
	}
}

static void legacy_isr(const struct device *dev)
{
	struct aicp_virtio_mmio_data *data = DEV_DATA(dev);
	k_spinlock_key_t key = k_spin_lock(&data->isr_lock);
	const uint32_t status = legacy_read32(dev, VIRTIO_MMIO_INTERRUPT_STATUS);

	if (status != 0u) {
		legacy_write32(dev, VIRTIO_MMIO_INTERRUPT_ACK, status);
		atomic_or(&data->pending_isr_status, (atomic_val_t)status);
		++data->irq_count;
		if (data->irq_count <= AICP_VIRTIO_TRACE_LIMIT) {
			LOG_INF("legacy irq count=%u status=0x%x", data->irq_count,
				status);
		}
		k_sem_give(&data->completion_sem);
	}
	k_spin_unlock(&data->isr_lock, key);
}

static struct virtq *legacy_get_virtqueue(const struct device *dev,
					  uint16_t queue_idx)
{
	struct aicp_virtio_mmio_data *data = DEV_DATA(dev);

	return queue_idx < data->virtqueue_count ? &data->virtqueues[queue_idx] : NULL;
}

static void legacy_notify_virtqueue(const struct device *dev, uint16_t queue_idx)
{
	struct aicp_virtio_mmio_data *data = DEV_DATA(dev);
	k_spinlock_key_t key = k_spin_lock(&data->notify_lock);
	struct virtq *vq = legacy_get_virtqueue(dev, queue_idx);
	uint16_t avail_idx = 0u;
	uint16_t desc_idx = 0u;
	const struct virtq_desc *desc = NULL;

	if (vq != NULL && vq->num != 0u) {
		avail_idx = sys_le16_to_cpu(vq->avail->idx);
		if (avail_idx != 0u) {
			const uint16_t ring_idx = (uint16_t)((avail_idx - 1u) % vq->num);

			desc_idx = sys_le16_to_cpu(vq->avail->ring[ring_idx]);
			if (desc_idx < vq->num) {
				desc = &vq->desc[desc_idx];
			}
		}
	}

	barrier_dmem_fence_full();
	legacy_write32(dev, VIRTIO_MMIO_QUEUE_NOTIFY, queue_idx);
	++data->notify_count;
	if (data->notify_count <= AICP_VIRTIO_TRACE_LIMIT && vq != NULL) {
		LOG_INF("legacy notify count=%u queue=%u avail=%u used=%u seen=%u desc=%u",
			data->notify_count, queue_idx,
			avail_idx, sys_le16_to_cpu(vq->used->idx),
			vq->last_used_idx, desc_idx);
		if (desc != NULL) {
			LOG_INF("legacy desc queue=%u index=%u addr=0x%llx len=%u flags=0x%x next=%u",
				queue_idx, desc_idx,
				(unsigned long long)sys_le64_to_cpu(desc->addr),
				sys_le32_to_cpu(desc->len),
				sys_le16_to_cpu(desc->flags),
				sys_le16_to_cpu(desc->next));
		} else if (avail_idx != 0u) {
			LOG_ERR("legacy invalid descriptor queue=%u index=%u size=%u",
				queue_idx, desc_idx, vq->num);
		}
	}
	k_spin_unlock(&data->notify_lock, key);
}

static void *legacy_get_device_specific_config(const struct device *dev)
{
	const mem_addr_t reg = DEVICE_MMIO_NAMED_GET(dev, reg_base) + VIRTIO_MMIO_CONFIG;

	barrier_dmem_fence_full();
	return (void *)reg;
}

static bool legacy_read_device_feature_bit(const struct device *dev, int bit)
{
	if (!IN_RANGE(bit, 0, 31)) {
		return false;
	}

	legacy_write32(dev, VIRTIO_MMIO_DEVICE_FEATURES_SEL, 0u);
	return (legacy_read32(dev, VIRTIO_MMIO_DEVICE_FEATURES) & BIT(bit)) != 0u;
}

static int legacy_write_driver_feature_bit(const struct device *dev, int bit,
					   bool value)
{
	struct aicp_virtio_mmio_data *data = DEV_DATA(dev);

	if (!IN_RANGE(bit, 0, 31)) {
		return -EINVAL;
	}

	if (value) {
		data->driver_features |= BIT(bit);
	} else {
		data->driver_features &= ~BIT(bit);
	}
	legacy_write32(dev, VIRTIO_MMIO_DRIVER_FEATURES_SEL, 0u);
	legacy_write32(dev, VIRTIO_MMIO_DRIVER_FEATURES, data->driver_features);
	return 0;
}

static int legacy_commit_feature_bits(const struct device *dev)
{
	/*
	 * QEMU accepts FEATURES_OK for its legacy MMIO transport, and the
	 * already verified RT-Thread guest advances through the same state.
	 * Keeping this transition also lets us detect rejected feature sets
	 * before queue activation instead of leaving the device half started.
	 */
	legacy_write32(dev, VIRTIO_MMIO_STATUS,
		       legacy_read32(dev, VIRTIO_MMIO_STATUS) |
		       BIT(DEVICE_STATUS_FEATURES_OK));
	if ((legacy_read32(dev, VIRTIO_MMIO_STATUS) &
	     BIT(DEVICE_STATUS_FEATURES_OK)) == 0u) {
		LOG_ERR("legacy device rejected negotiated features=0x%x",
			DEV_DATA(dev)->driver_features);
		return -ENOTSUP;
	}
	LOG_INF("legacy FEATURES_OK status=0x%x features=0x%x",
		legacy_read32(dev, VIRTIO_MMIO_STATUS),
		DEV_DATA(dev)->driver_features);
	return 0;
}

static void legacy_set_status_bit(const struct device *dev, int bit)
{
	legacy_write32(dev, VIRTIO_MMIO_STATUS,
		       legacy_read32(dev, VIRTIO_MMIO_STATUS) | BIT(bit));
}

static int legacy_virtq_create(struct virtq *vq, void **queue_area, uint16_t size,
			       uint16_t usable_descs)
{
	const size_t descriptor_size = 16u * size;
	const size_t available_size = 6u + 2u * size;
	const size_t used_offset = ROUND_UP(descriptor_size + available_size,
						  AICP_VIRTIO_LEGACY_PAGE_SIZE);
	const size_t used_size = 6u + 8u * size;
	const size_t shared_size = ROUND_UP(used_offset + used_size,
						  AICP_VIRTIO_LEGACY_PAGE_SIZE);
	uint8_t *shared;
	int ret;

	if (!is_power_of_two(size) || size == 0u || size > 32768u ||
	    usable_descs == 0u || usable_descs > size) {
		return -EINVAL;
	}

	shared = k_aligned_alloc(AICP_VIRTIO_LEGACY_PAGE_SIZE, shared_size);
	if (shared == NULL) {
		return -ENOMEM;
	}
	memset(shared, 0, shared_size);
	memset(vq, 0, sizeof(*vq));

	vq->recv_cbs = k_calloc(size, sizeof(*vq->recv_cbs));
	if (vq->recv_cbs == NULL) {
		k_free(shared);
		return -ENOMEM;
	}

	ret = k_stack_alloc_init(&vq->free_desc_stack, size);
	if (ret != 0) {
		k_free(vq->recv_cbs);
		k_free(shared);
		return ret;
	}

	vq->num = size;
	vq->desc = (struct virtq_desc *)shared;
	vq->avail = (struct virtq_avail *)(shared + descriptor_size);
	vq->used = (struct virtq_used *)(shared + used_offset);
	vq->last_used_idx = 0u;
	/*
	 * The ring can be larger than the capacity selected by the function
	 * driver, but descriptors beyond that capacity must never be offered.
	 * Zephyr's virtio-net TX path requests one descriptor because every
	 * packet uses the same backing buffer; exposing more permits an in-flight
	 * packet to be overwritten and corrupts TCP handshakes.
	 */
	for (uint16_t i = 0; i < usable_descs; ++i) {
		k_stack_push(&vq->free_desc_stack, i);
	}
	vq->free_desc_n = usable_descs;
	*queue_area = shared;
	return 0;
}

static void legacy_virtq_destroy(struct virtq *vq, void *queue_area)
{
	if (vq->recv_cbs != NULL) {
		k_free(vq->recv_cbs);
	}
	k_stack_cleanup(&vq->free_desc_stack);
	k_free(queue_area);
}

static int legacy_activate_virtqueue(const struct device *dev, uint16_t queue_idx,
				    struct virtq *vq)
{
	const uintptr_t queue_phys = k_mem_phys_addr(vq->desc);

	legacy_write32(dev, VIRTIO_MMIO_QUEUE_SEL, queue_idx);
	const uint32_t max_size = legacy_read32(dev, VIRTIO_MMIO_QUEUE_SIZE_MAX);
	if (max_size < vq->num) {
		LOG_ERR("%s queue %u supports %u entries, requested %u", dev->name,
			queue_idx, max_size, vq->num);
		return -EINVAL;
	}
	if (!IS_ALIGNED(queue_phys, AICP_VIRTIO_LEGACY_PAGE_SIZE) ||
	    (queue_phys >> 32) != 0u) {
		LOG_ERR("%s queue %u physical address 0x%lx is not a legacy PFN",
			dev->name, queue_idx, (unsigned long)queue_phys);
		return -EINVAL;
	}

	legacy_write32(dev, VIRTIO_MMIO_QUEUE_SIZE, vq->num);
	legacy_write32(dev, AICP_VIRTIO_MMIO_QUEUE_ALIGN,
		       AICP_VIRTIO_LEGACY_PAGE_SIZE);
	legacy_write32(dev, AICP_VIRTIO_MMIO_QUEUE_PFN,
		       (uint32_t)(queue_phys / AICP_VIRTIO_LEGACY_PAGE_SIZE));
	const uint32_t queue_pfn = legacy_read32(dev, AICP_VIRTIO_MMIO_QUEUE_PFN);
	LOG_INF("legacy queue=%u size=%u phys=0x%lx used=0x%lx pfn=0x%x",
		queue_idx,
		vq->num, (unsigned long)queue_phys,
		(unsigned long)k_mem_phys_addr(vq->used), queue_pfn);
	if (queue_pfn != (uint32_t)(queue_phys / AICP_VIRTIO_LEGACY_PAGE_SIZE)) {
		LOG_ERR("legacy queue=%u PFN readback mismatch expected=0x%lx actual=0x%x",
			queue_idx,
			(unsigned long)(queue_phys / AICP_VIRTIO_LEGACY_PAGE_SIZE),
			queue_pfn);
		return -EIO;
	}
	return 0;
}

static int legacy_init_virtqueues(const struct device *dev, uint16_t queue_count,
				  virtio_enumerate_queues enumerate, void *opaque)
{
	struct aicp_virtio_mmio_data *data = DEV_DATA(dev);
	uint16_t created = 0u;
	int ret = 0;

	data->virtqueues = k_calloc(queue_count, sizeof(*data->virtqueues));
	data->queue_areas = k_calloc(queue_count, sizeof(*data->queue_areas));
	if (data->virtqueues == NULL || data->queue_areas == NULL) {
		ret = -ENOMEM;
		goto fail;
	}
	data->virtqueue_count = queue_count;

	legacy_write32(dev, AICP_VIRTIO_MMIO_GUEST_PAGE_SIZE,
		       AICP_VIRTIO_LEGACY_PAGE_SIZE);
	for (uint16_t i = 0; i < queue_count; ++i) {
		legacy_write32(dev, VIRTIO_MMIO_QUEUE_SEL, i);
		const uint16_t max_size =
			(uint16_t)legacy_read32(dev, VIRTIO_MMIO_QUEUE_SIZE_MAX);
		const uint16_t requested_size = enumerate(i, max_size, opaque);
		if (requested_size == 0u || requested_size > max_size ||
		    !is_power_of_two(requested_size)) {
			LOG_ERR("%s queue %u requested invalid size %u (max %u)",
				dev->name, i, requested_size, max_size);
			ret = -EINVAL;
			goto fail;
		}
		const uint16_t queue_size =
			MIN(max_size, MAX(requested_size,
					  AICP_VIRTIO_LEGACY_MIN_QUEUE_SIZE));
		const uint16_t usable_descs = requested_size;

		LOG_INF("legacy queue request index=%u requested=%u selected=%u usable=%u max=%u",
			i, requested_size, queue_size, usable_descs, max_size);

		ret = legacy_virtq_create(&data->virtqueues[i],
					  &data->queue_areas[i], queue_size,
					  usable_descs);
		if (ret != 0) {
			goto fail;
		}
		++created;
		ret = legacy_activate_virtqueue(dev, i, &data->virtqueues[i]);
		if (ret != 0) {
			goto fail;
		}
	}
	return 0;

fail:
	for (uint16_t i = 0; i < created; ++i) {
		legacy_write32(dev, VIRTIO_MMIO_QUEUE_SEL, i);
		legacy_write32(dev, AICP_VIRTIO_MMIO_QUEUE_PFN, 0u);
		legacy_virtq_destroy(&data->virtqueues[i], data->queue_areas[i]);
	}
	k_free(data->queue_areas);
	k_free(data->virtqueues);
	data->queue_areas = NULL;
	data->virtqueues = NULL;
	data->virtqueue_count = 0u;
	return ret;
}

static void legacy_finalize_init(const struct device *dev)
{
	struct aicp_virtio_mmio_data *data = DEV_DATA(dev);

	legacy_set_status_bit(dev, DEVICE_STATUS_DRIVER_OK);
	LOG_INF("legacy DRIVER_OK status=0x%x", legacy_read32(dev,
						       VIRTIO_MMIO_STATUS));
	(void)k_thread_create(&data->completion_thread, data->completion_stack,
			      K_KERNEL_STACK_SIZEOF(data->completion_stack),
			      legacy_completion_thread, data, NULL, NULL,
			      AICP_VIRTIO_COMPLETION_PRIORITY, 0, K_NO_WAIT);
}

static DEVICE_API(virtio, legacy_driver_api) = {
	.get_virtqueue = legacy_get_virtqueue,
	.notify_virtqueue = legacy_notify_virtqueue,
	.get_device_specific_config = legacy_get_device_specific_config,
	.read_device_feature_bit = legacy_read_device_feature_bit,
	.write_driver_feature_bit = legacy_write_driver_feature_bit,
	.commit_feature_bits = legacy_commit_feature_bits,
	.init_virtqueues = legacy_init_virtqueues,
	.finalize_init = legacy_finalize_init,
};

static int legacy_init_common(const struct device *dev)
{
	struct aicp_virtio_mmio_data *data = DEV_DATA(dev);
	uint32_t offered_features;
	uint32_t version;

	DEVICE_MMIO_NAMED_MAP(dev, reg_base, K_MEM_CACHE_NONE);
	if (legacy_read32(dev, VIRTIO_MMIO_MAGIC_VALUE) !=
	    AICP_VIRTIO_MMIO_MAGIC) {
		LOG_ERR("invalid virtio-mmio magic");
		return -EINVAL;
	}
	version = legacy_read32(dev, VIRTIO_MMIO_VERSION);
	if (version != AICP_VIRTIO_MMIO_LEGACY_VERSION) {
		LOG_ERR("expected legacy virtio-mmio version 1, got %u", version);
		return -EINVAL;
	}
	if (legacy_read32(dev, VIRTIO_MMIO_DEVICE_ID) ==
	    AICP_VIRTIO_MMIO_INVALID_DEVICE_ID) {
		LOG_ERR("invalid virtio-mmio device id");
		return -EINVAL;
	}

	legacy_write32(dev, VIRTIO_MMIO_STATUS, 0u);
	while (legacy_read32(dev, VIRTIO_MMIO_STATUS) != 0u) {
		k_busy_wait(1u);
	}
	legacy_set_status_bit(dev, DEVICE_STATUS_ACKNOWLEDGE);
	legacy_set_status_bit(dev, DEVICE_STATUS_DRIVER);
	legacy_write32(dev, VIRTIO_MMIO_DEVICE_FEATURES_SEL, 0u);
	legacy_write32(dev, VIRTIO_MMIO_DRIVER_FEATURES_SEL, 0u);
	offered_features = legacy_read32(dev, VIRTIO_MMIO_DEVICE_FEATURES);
	data->driver_features = 0u;
	data->dev = dev;
	atomic_clear(&data->pending_isr_status);
	k_sem_init(&data->completion_sem, 0, 1);

	/* Zephyr's virtio-net header includes num_buffers, so negotiate it. */
	if (!legacy_read_device_feature_bit(dev, AICP_VIRTIO_NET_F_MRG_RXBUF)) {
		LOG_ERR("legacy virtio-net device does not offer MRG_RXBUF");
		return -ENOTSUP;
	}
	legacy_write_driver_feature_bit(dev, AICP_VIRTIO_NET_F_MRG_RXBUF, true);
	/*
	 * Zephyr places the virtio-net header and Ethernet frame in one
	 * descriptor. Legacy virtio requires ANY_LAYOUT for that buffer shape;
	 * without it QEMU expects the header and payload in separate descriptors.
	 */
	if (!legacy_read_device_feature_bit(dev, AICP_VIRTIO_F_ANY_LAYOUT)) {
		LOG_ERR("legacy virtio-net device does not offer ANY_LAYOUT");
		return -ENOTSUP;
	}
	legacy_write_driver_feature_bit(dev, AICP_VIRTIO_F_ANY_LAYOUT, true);
	LOG_INF("legacy virtio-mmio ready device=%u vendor=0x%x offered=0x%x features=0x%x",
		legacy_read32(dev, VIRTIO_MMIO_DEVICE_ID),
		legacy_read32(dev, VIRTIO_MMIO_VENDOR_ID), offered_features,
		data->driver_features);
	return 0;
}

#define AICP_VIRTIO_MMIO_DEFINE(inst)                                                \
	static struct aicp_virtio_mmio_data legacy_data_##inst;                       \
	static const struct aicp_virtio_mmio_config legacy_config_##inst = {           \
		DEVICE_MMIO_NAMED_ROM_INIT(reg_base, DT_DRV_INST(inst)),                 \
	};                                                                               \
	static int legacy_init_##inst(const struct device *dev)                         \
	{                                                                                \
		IRQ_CONNECT(DT_INST_IRQN(inst), DT_INST_IRQ(inst, priority),             \
			    legacy_isr, DEVICE_DT_INST_GET(inst), 0);                    \
		const int ret = legacy_init_common(dev);                                  \
		if (ret == 0) {                                                             \
			irq_enable(DT_INST_IRQN(inst));                                      \
		}                                                                          \
		return ret;                                                                \
	}                                                                                \
	DEVICE_DT_INST_DEFINE(inst, legacy_init_##inst, NULL, &legacy_data_##inst,       \
			      &legacy_config_##inst, POST_KERNEL, 0, &legacy_driver_api);

DT_INST_FOREACH_STATUS_OKAY(AICP_VIRTIO_MMIO_DEFINE)
