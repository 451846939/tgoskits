/*
 * Copyright (c) 2006-2021, RT-Thread Development Team
 * Copyright 2026 The TGOSKits Authors
 *
 * SPDX-License-Identifier: Apache-2.0
 */

#include <rtthread.h>
#include <cpuport.h>
#include <interrupt.h>

#include <virtio.h>
#ifdef BSP_USING_VIRTIO_BLK
#include <virtio_blk.h>
#endif
#ifdef BSP_USING_VIRTIO_NET
#include <virtio_net.h>
#endif
#ifdef BSP_USING_VIRTIO_CONSOLE
#include <virtio_console.h>
#endif
#ifdef BSP_USING_VIRTIO_GPU
#include <virtio_gpu.h>
#endif
#ifdef BSP_USING_VIRTIO_INPUT
#include <virtio_input.h>
#endif

#include <board.h>

#ifdef BSP_USING_VIRTIO_NET
#define AICP_VIRTIO_NET_DEVICE "virtio-net0"
#define AICP_FRAME_TRACE_LIMIT 8
#define AICP_TX_SLOT_COUNT      (VIRTIO_NET_RTX_QUEUE_SIZE / 2)
#define AICP_TX_WAIT_ROUNDS     2000
#define AICP_QUEUE_MONITOR_INTERVAL_MS 100
#define AICP_QUEUE_MONITOR_LOG_ROUNDS  300
#define AICP_QUEUE_MONITOR_PRIORITY    (RT_THREAD_PRIORITY_MAX - 2)

static struct virtio_net_device *aicp_net_device;
static rt_uint32_t aicp_net_irq;
static volatile rt_uint32_t aicp_net_isr_count;
static rt_uint32_t aicp_rx_frame_count;
static rt_uint32_t aicp_tx_frame_count;
static struct pbuf *(*aicp_original_eth_rx)(rt_device_t dev);
static struct rt_mutex aicp_tx_lock;
static rt_bool_t aicp_tx_slot_busy[AICP_TX_SLOT_COUNT];

static void aicp_trace_frame(const char *direction, rt_uint32_t count,
                             const struct pbuf *p)
{
    const rt_uint8_t *data;

    if (count > AICP_FRAME_TRACE_LIMIT || p == RT_NULL || p->len < 14)
    {
        return;
    }

    data = (const rt_uint8_t *)p->payload;
    rt_kprintf("AICP_RTTHREAD_%s_FRAME count=%u len=%u "
               "dst=%02x:%02x:%02x:%02x:%02x:%02x "
               "src=%02x:%02x:%02x:%02x:%02x:%02x eth=0x%02x%02x",
               direction, count, p->tot_len,
               data[0], data[1], data[2], data[3], data[4], data[5],
               data[6], data[7], data[8], data[9], data[10], data[11],
               data[12], data[13]);
    if (p->len >= 34 && data[12] == 0x08 && data[13] == 0x00)
    {
        rt_kprintf(" ip=%u.%u.%u.%u->%u.%u.%u.%u proto=%u",
                   data[26], data[27], data[28], data[29],
                   data[30], data[31], data[32], data[33], data[23]);
    }
    rt_kprintf("\n");
}

static struct pbuf *aicp_virtio_net_rx(rt_device_t dev)
{
    struct pbuf *p = aicp_original_eth_rx(dev);

    if (p != RT_NULL)
    {
        ++aicp_rx_frame_count;
        aicp_trace_frame("RX", aicp_rx_frame_count, p);
    }
    return p;
}

static rt_uint16_t aicp_virtio_net_reclaim_tx(struct virtq *tx)
{
    rt_uint16_t reclaimed = 0;

    rt_hw_dsb();
    while (tx->used_idx != tx->used->idx)
    {
        const rt_uint16_t used_index = tx->used_idx % tx->num;
        const rt_uint32_t head = tx->used->ring[used_index].id;

        rt_hw_dsb();
        ++tx->used_idx;
        ++reclaimed;
        if (head < tx->num && (head % 2) == 0)
        {
            aicp_tx_slot_busy[head / 2] = RT_FALSE;
        }
        else
        {
            rt_kprintf("AICP_RTTHREAD_TX_BAD_USED id=%u used_index=%u\n",
                       head, used_index);
        }
    }

    return reclaimed;
}

static int aicp_virtio_net_find_tx_slot(void)
{
    int slot;

    for (slot = 0; slot < AICP_TX_SLOT_COUNT; ++slot)
    {
        if (!aicp_tx_slot_busy[slot])
        {
            return slot;
        }
    }
    return -1;
}

static void aicp_virtio_net_dump_tx(const struct virtq *tx)
{
    rt_uint16_t slot;
    rt_uint16_t index;

    rt_kprintf("AICP_RTTHREAD_TX_DUMP num=%u avail=%u used=%u seen=%u "
               "avail_flags=0x%x used_flags=0x%x\n",
               tx->num, tx->avail->idx, tx->used->idx, tx->used_idx,
               tx->avail->flags, tx->used->flags);
    for (slot = 0; slot < AICP_TX_SLOT_COUNT; ++slot)
    {
        const rt_uint16_t head = slot * 2;
        const struct virtq_desc *header_desc = &tx->desc[head];
        const struct virtq_desc *payload_desc = &tx->desc[head + 1];

        rt_kprintf("AICP_RTTHREAD_TX_SLOT slot=%u busy=%u head=%u "
                   "d0_addr=0x%llx d0_len=%u d0_flags=0x%x d0_next=%u "
                   "d1_addr=0x%llx d1_len=%u d1_flags=0x%x d1_next=%u\n",
                   slot, aicp_tx_slot_busy[slot], head,
                   (unsigned long long)header_desc->addr, header_desc->len,
                   header_desc->flags, header_desc->next,
                   (unsigned long long)payload_desc->addr, payload_desc->len,
                   payload_desc->flags, payload_desc->next);
    }
    for (index = 0; index < tx->num; ++index)
    {
        rt_kprintf("AICP_RTTHREAD_TX_RING index=%u avail_head=%u used_id=%u "
                   "used_len=%u\n",
                   index, tx->avail->ring[index], tx->used->ring[index].id,
                   tx->used->ring[index].len);
    }
}

static rt_err_t aicp_virtio_net_tx(rt_device_t dev, struct pbuf *p)
{
    struct virtio_net_device *net = (struct virtio_net_device *)dev;
    struct virtio_device *virtio = &net->virtio_dev;
    struct virtq *tx = &virtio->queues[VIRTIO_NET_QUEUE_TX];
    int slot;
    int wait_round;
    rt_uint16_t head;

    if (p == RT_NULL || p->tot_len > VIRTIO_NET_PAYLOAD_MAX_SIZE)
    {
        return -RT_EINVAL;
    }

    if (rt_mutex_take(&aicp_tx_lock, RT_WAITING_FOREVER) != RT_EOK)
    {
        return -RT_ERROR;
    }

    slot = -1;
    for (wait_round = 0; wait_round < AICP_TX_WAIT_ROUNDS; ++wait_round)
    {
        aicp_virtio_net_reclaim_tx(tx);
        slot = aicp_virtio_net_find_tx_slot();
        if (slot >= 0)
        {
            break;
        }
        rt_thread_mdelay(1);
    }
    if (slot < 0)
    {
        rt_kprintf("AICP_RTTHREAD_TX_TIMEOUT avail=%u used=%u seen=%u\n",
                   tx->avail->idx, tx->used->idx, tx->used_idx);
        aicp_virtio_net_dump_tx(tx);
        rt_mutex_release(&aicp_tx_lock);
        return -RT_ETIMEOUT;
    }

    head = (rt_uint16_t)(slot * 2);
    aicp_tx_slot_busy[slot] = RT_TRUE;
    rt_memset(&net->info[head].hdr, 0, sizeof(net->info[head].hdr));
    pbuf_copy_partial(p, net->info[head].rx_buffer, p->tot_len, 0);

    virtio_fill_desc(virtio, VIRTIO_NET_QUEUE_TX, head,
                     VIRTIO_VA2PA(&net->info[head].hdr),
                     VIRTIO_NET_HDR_SIZE, VIRTQ_DESC_F_NEXT, head + 1);
    virtio_fill_desc(virtio, VIRTIO_NET_QUEUE_TX, head + 1,
                     VIRTIO_VA2PA(net->info[head].rx_buffer), p->tot_len,
                     0, 0);
    virtio_submit_chain(virtio, VIRTIO_NET_QUEUE_TX, head);
    virtio_queue_notify(virtio, VIRTIO_NET_QUEUE_TX);

    ++aicp_tx_frame_count;
    aicp_trace_frame("TX", aicp_tx_frame_count, p);
    rt_mutex_release(&aicp_tx_lock);
    return RT_EOK;
}

static void aicp_virtio_net_isr(int irqno, void *param)
{
    struct virtio_net_device *net = (struct virtio_net_device *)param;
    struct virtio_device *dev = &net->virtio_dev;
    struct virtq *rx = &dev->queues[VIRTIO_NET_QUEUE_RX];

    RT_UNUSED(irqno);
    ++aicp_net_isr_count;
    virtio_interrupt_ack(dev);
    rt_hw_dsb();

    if (rx->used_idx != rx->used->idx)
    {
        rt_hw_dsb();
        eth_device_ready(&net->parent);
    }
}

static void aicp_virtio_queue_monitor(void *parameter)
{
    struct virtio_device *dev = &aicp_net_device->virtio_dev;
    struct virtq *rx = &dev->queues[VIRTIO_NET_QUEUE_RX];
    struct virtq *tx = &dev->queues[VIRTIO_NET_QUEUE_TX];
    rt_uint32_t round = 0;

    RT_UNUSED(parameter);

    while (RT_TRUE)
    {
        rt_uint16_t rx_pending;
        rt_uint16_t tx_reclaimed = 0;

        rt_thread_mdelay(AICP_QUEUE_MONITOR_INTERVAL_MS);
        ++round;
        rt_hw_dsb();
        rx_pending = (rt_uint16_t)(rx->used->idx - rx->used_idx);

        if (rx_pending != 0)
        {
            eth_device_ready(&aicp_net_device->parent);
        }

        if (rt_mutex_take(&aicp_tx_lock, RT_WAITING_FOREVER) == RT_EOK)
        {
            tx_reclaimed = aicp_virtio_net_reclaim_tx(tx);
            rt_mutex_release(&aicp_tx_lock);
        }

        if (rx_pending != 0 || tx_reclaimed != 0 ||
            round % AICP_QUEUE_MONITOR_LOG_ROUNDS == 0)
        {
            rt_kprintf("AICP_RTTHREAD_VQ round=%u irq=%u status=0x%x isr=%u "
                       "rx_avail=%u rx_used=%u rx_seen=%u pending=%u "
                       "tx_avail=%u tx_used=%u tx_seen=%u reclaimed=%u\n",
                       round, aicp_net_irq,
                       dev->mmio_config->interrupt_status,
                       aicp_net_isr_count, rx->avail->idx, rx->used->idx,
                       rx->used_idx, rx_pending, tx->avail->idx,
                       tx->used->idx, tx->used_idx, tx_reclaimed);
        }
    }
}

static rt_err_t aicp_install_virtio_net_probe(rt_uint32_t irq)
{
    rt_device_t device = rt_device_find(AICP_VIRTIO_NET_DEVICE);
    rt_thread_t queue_monitor;
    struct virtq *tx;
    rt_uint16_t index;

    if (device == RT_NULL)
    {
        rt_kprintf("AICP_RTTHREAD_VIRTIO_FATAL stage=find_net_device name=%s\n",
                   AICP_VIRTIO_NET_DEVICE);
        return -RT_ERROR;
    }

    aicp_net_device = (struct virtio_net_device *)device;
    aicp_net_irq = irq;
    aicp_original_eth_rx = aicp_net_device->parent.eth_rx;
    tx = &aicp_net_device->virtio_dev.queues[VIRTIO_NET_QUEUE_TX];
    rt_memset(aicp_tx_slot_busy, 0, sizeof(aicp_tx_slot_busy));
    tx->used_idx = tx->used->idx;
    for (index = tx->used->idx; index != tx->avail->idx; ++index)
    {
        const rt_uint16_t head = tx->avail->ring[index % tx->num];

        if (head < tx->num && (head % 2) == 0)
        {
            aicp_tx_slot_busy[head / 2] = RT_TRUE;
        }
    }
    if (rt_mutex_init(&aicp_tx_lock, "aicp-tx", RT_IPC_FLAG_PRIO) != RT_EOK)
    {
        rt_kprintf("AICP_RTTHREAD_VIRTIO_FATAL stage=init_tx_lock\n");
        return -RT_ERROR;
    }
    aicp_net_device->parent.eth_rx = aicp_virtio_net_rx;
    aicp_net_device->parent.eth_tx = aicp_virtio_net_tx;
    rt_hw_interrupt_install(irq, aicp_virtio_net_isr, aicp_net_device,
                            "aicp-vnet");
    rt_hw_interrupt_umask(irq);
    queue_monitor = rt_thread_create("aicp-vq", aicp_virtio_queue_monitor,
                                     RT_NULL, 4096,
                                     AICP_QUEUE_MONITOR_PRIORITY, 10);
    if (queue_monitor == RT_NULL)
    {
        rt_kprintf("AICP_RTTHREAD_VIRTIO_FATAL stage=create_queue_monitor\n");
        return -RT_ENOMEM;
    }
    rt_thread_startup(queue_monitor);
    rt_kprintf("AICP_RTTHREAD_IRQ_PROBE irq=%u device=%s\n",
               irq, AICP_VIRTIO_NET_DEVICE);
    return RT_EOK;
}

rt_err_t aicp_virtio_net_publish_link_up(void)
{
    if (aicp_net_device == RT_NULL || aicp_net_device->parent.netif == RT_NULL)
    {
        return -RT_EBUSY;
    }
    return eth_device_linkchange(&aicp_net_device->parent, RT_TRUE);
}

void aicp_virtio_net_get_stats(rt_uint32_t *irq_count,
                               rt_uint32_t *rx_frames,
                               rt_uint32_t *tx_frames)
{
    if (irq_count != RT_NULL)
    {
        *irq_count = aicp_net_isr_count;
    }
    if (rx_frames != RT_NULL)
    {
        *rx_frames = aicp_rx_frame_count;
    }
    if (tx_frames != RT_NULL)
    {
        *tx_frames = aicp_tx_frame_count;
    }
}
#endif

static virtio_device_init_handler virtio_device_init_handlers[] =
{
#ifdef BSP_USING_VIRTIO_BLK
    [VIRTIO_DEVICE_ID_BLOCK] = rt_virtio_blk_init,
#endif
#ifdef BSP_USING_VIRTIO_NET
    [VIRTIO_DEVICE_ID_NET] = rt_virtio_net_init,
#endif
#ifdef BSP_USING_VIRTIO_CONSOLE
    [VIRTIO_DEVICE_ID_CONSOLE] = rt_virtio_console_init,
#endif
#ifdef BSP_USING_VIRTIO_GPU
    [VIRTIO_DEVICE_ID_GPU] = rt_virtio_gpu_init,
#endif
#ifdef BSP_USING_VIRTIO_INPUT
    [VIRTIO_DEVICE_ID_INPUT] = rt_virtio_input_init,
#endif
};

int rt_virtio_devices_init(void)
{
    int i;
    rt_uint32_t irq = VIRTIO_IRQ_BASE;
    rt_ubase_t phys_base = VIRTIO_MMIO_BASE;
    rt_ubase_t mmio_base;
    struct virtio_mmio_config *mmio_config;
    virtio_device_init_handler init_handler;
    const rt_size_t handler_count =
        sizeof(virtio_device_init_handlers) / sizeof(virtio_device_init_handlers[0]);

    if (handler_count == 0)
    {
        return 0;
    }

    mmio_base = (rt_ubase_t)rt_ioremap((void *)phys_base,
                                       VIRTIO_MMIO_SIZE * VIRTIO_MAX_NR);
    rt_kprintf("AICP_RTTHREAD_VIRTIO_MAP phys=0x%lx virt=0x%lx size=0x%lx count=%d irq_base=%u\n",
               phys_base, mmio_base,
               (rt_ubase_t)(VIRTIO_MMIO_SIZE * VIRTIO_MAX_NR),
               VIRTIO_MAX_NR, VIRTIO_IRQ_BASE);

    if (mmio_base == (rt_ubase_t)RT_NULL)
    {
        rt_kprintf("AICP_RTTHREAD_VIRTIO_FATAL stage=ioremap\n");
        return -RT_ERROR;
    }

    for (i = 0; i < VIRTIO_MAX_NR;
         ++i, ++irq, phys_base += VIRTIO_MMIO_SIZE, mmio_base += VIRTIO_MMIO_SIZE)
    {
        rt_err_t ret;

        mmio_config = (struct virtio_mmio_config *)mmio_base;
        rt_kprintf("AICP_RTTHREAD_VIRTIO_PROBE slot=%d phys=0x%lx virt=0x%lx "
                   "magic=0x%08x version=%u device=%u vendor=0x%08x irq=%u\n",
                   i, phys_base, mmio_base, mmio_config->magic,
                   mmio_config->version, mmio_config->device_id,
                   mmio_config->vendor_id, irq);

        if (mmio_config->magic != VIRTIO_MAGIC_VALUE ||
            mmio_config->version != RT_USING_VIRTIO_VERSION ||
            mmio_config->vendor_id != VIRTIO_VENDOR_ID)
        {
            continue;
        }

        if (mmio_config->device_id >= handler_count)
        {
            rt_kprintf("AICP_RTTHREAD_VIRTIO_SKIP slot=%d reason=device_id_range device=%u\n",
                       i, mmio_config->device_id);
            continue;
        }

        init_handler = virtio_device_init_handlers[mmio_config->device_id];
        if (init_handler == RT_NULL)
        {
            rt_kprintf("AICP_RTTHREAD_VIRTIO_SKIP slot=%d reason=no_handler device=%u\n",
                       i, mmio_config->device_id);
            continue;
        }

        ret = init_handler((rt_ubase_t *)mmio_base, irq);
        rt_kprintf("AICP_RTTHREAD_VIRTIO_INIT slot=%d device=%u irq=%u ret=%d\n",
                   i, mmio_config->device_id, irq, ret);
#ifdef BSP_USING_VIRTIO_NET
        if (ret == RT_EOK && mmio_config->device_id == VIRTIO_DEVICE_ID_NET)
        {
            ret = aicp_install_virtio_net_probe(irq);
            rt_kprintf("AICP_RTTHREAD_IRQ_PROBE_INIT irq=%u ret=%d\n",
                       irq, ret);
        }
#endif
    }

    return 0;
}
INIT_DEVICE_EXPORT(rt_virtio_devices_init);
