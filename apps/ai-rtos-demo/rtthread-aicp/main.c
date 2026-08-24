// Copyright 2026 The TGOSKits Authors
//
// SPDX-License-Identifier: Apache-2.0

#include <arpa/inet.h>
#include <errno.h>
#include <netdev.h>
#include <netinet/in.h>
#include <rtthread.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <unistd.h>

#include "aicp_service.h"
#include "gtimer.h"

#define AICP_PORT 8800
#define NETWORK_WAIT_ROUNDS 300
#define AICP_RTTHREAD_STATIC_IP "10.0.3.2"
#define AICP_RTTHREAD_STATIC_NETMASK "255.255.255.0"
#define AICP_RTTHREAD_STATIC_GATEWAY "10.0.3.1"

void aicp_virtio_net_get_stats(rt_uint32_t *irq_count,
                               rt_uint32_t *rx_frames,
                               rt_uint32_t *tx_frames);

static rt_uint32_t accepted_clients;
static rt_uint32_t disconnected_clients;
static struct aicp_service_stats service_stats;

static ptrdiff_t rtthread_stream_read(void *context, void *buffer, size_t length)
{
    const int fd = *(const int *)context;
    const ssize_t result = recv(fd, buffer, length, 0);

    if (result >= 0)
    {
        return (ptrdiff_t)result;
    }
    return errno != 0 ? (ptrdiff_t)-errno : (ptrdiff_t)result;
}

static ptrdiff_t rtthread_stream_write(void *context,
                                       const void *buffer,
                                       size_t length)
{
    const int fd = *(const int *)context;
    const ssize_t result = send(fd, buffer, length, 0);

    if (result >= 0)
    {
        return (ptrdiff_t)result;
    }
    return errno != 0 ? (ptrdiff_t)-errno : (ptrdiff_t)result;
}

static void log_runtime_stats(const char *reason)
{
    rt_uint32_t irq_count = 0;
    rt_uint32_t rx_frames = 0;
    rt_uint32_t tx_frames = 0;

    aicp_virtio_net_get_stats(&irq_count, &rx_frames, &tx_frames);
    rt_kprintf("AICP_RTTHREAD_STATS reason=%s clients=%u disconnects=%u "
               "controls=%u errors=%u duplicates=%u stale=%u "
               "irq=%u rx_frames=%u tx_frames=%u\n",
               reason, accepted_clients, disconnected_clients,
               service_stats.control_requests, service_stats.protocol_errors,
               service_stats.duplicate_requests, service_stats.stale_requests,
               irq_count, rx_frames, tx_frames);
}

static rt_uint64_t monotonic_ns(void *context)
{
    (void)context;
    const rt_uint64_t cycles = rt_hw_get_cntpct_val();
    const rt_uint64_t frequency = rt_hw_get_gtimer_frq();

    return (cycles / frequency) * 1000000000ULL +
           (cycles % frequency) * 1000000000ULL / frequency;
}

static int configure_static_network(struct netdev *netdev)
{
    ip_addr_t ip;
    ip_addr_t netmask;
    ip_addr_t gateway;
    int ret;

    if (!inet_aton(AICP_RTTHREAD_STATIC_IP, &ip) ||
        !inet_aton(AICP_RTTHREAD_STATIC_NETMASK, &netmask) ||
        !inet_aton(AICP_RTTHREAD_STATIC_GATEWAY, &gateway))
    {
        return -RT_EINVAL;
    }

    ret = netdev_dhcp_enabled(netdev, RT_FALSE);
    if (ret == RT_EOK)
    {
        ret = netdev_set_ipaddr(netdev, &ip);
    }
    if (ret == RT_EOK)
    {
        ret = netdev_set_netmask(netdev, &netmask);
    }
    if (ret == RT_EOK)
    {
        ret = netdev_set_gw(netdev, &gateway);
    }
    if (ret == RT_EOK)
    {
        ret = netdev_set_up(netdev);
    }

    rt_kprintf("AICP_RTTHREAD_STATIC_NET ret=%d dev=%s ip=%s "
               "netmask=%s gateway=%s\n",
               ret, netdev->name, AICP_RTTHREAD_STATIC_IP,
               AICP_RTTHREAD_STATIC_NETMASK, AICP_RTTHREAD_STATIC_GATEWAY);
    return ret;
}

static void wait_for_network(void)
{
    rt_bool_t static_network_configured = RT_FALSE;

    for (int round = 0; round < NETWORK_WAIT_ROUNDS; ++round)
    {
        struct netdev *netdev = netdev_default;

        /*
         * The legacy VirtIO device can receive frames before lwIP reports
         * link-up.  Configure the deterministic QEMU address as soon as the
         * netdev is registered so startup does not depend on a timer wake-up
         * or a link-state notification that may arrive later.
         */
        if (netdev != RT_NULL && !static_network_configured)
        {
            static_network_configured = RT_TRUE;
            if (configure_static_network(netdev) != RT_EOK)
            {
                rt_kprintf("AICP_RTTHREAD_STATIC_NET_RETRY dev=%s\n",
                           netdev->name);
                static_network_configured = RT_FALSE;
            }
        }
        if (netdev != RT_NULL && netdev_is_up(netdev) &&
            !ip_addr_isany(&netdev->ip_addr))
        {
            rt_kprintf("AICP_RTTHREAD_NET_UP dev=%s ip=%s flags=0x%x link=%u\n",
                       netdev->name, inet_ntoa(netdev->ip_addr), netdev->flags,
                       netdev_is_link_up(netdev) ? 1U : 0U);
            return;
        }
        rt_thread_mdelay(100);
    }

    rt_kprintf("AICP_RTTHREAD_NET_TIMEOUT\n");
}

static void log_control(const struct control_state *state, rt_uint32_t seq)
{
    const int target_milli = (int)(state->setpoint * 1000.0f);
    const int measured_milli = (int)(state->measured * 1000.0f);
    const int output_milli = (int)(state->control_output * 1000.0f);

    rt_kprintf("AICP_RTTHREAD_CONTROL seq=%u target_milli=%d "
               "measured_milli=%d output_milli=%d mode=%u\n",
               seq, target_milli, measured_milli, output_milli, state->mode);
}

static void log_service_event(
    void *context,
    const struct aicp_service_event_data *event)
{
    (void)context;
    const struct aicp_header *header = event->header;

    switch (event->event)
    {
    case AICP_SERVICE_FRAME_RECEIVED:
        break;
    case AICP_SERVICE_HELLO:
        rt_kprintf("AICP_RTTHREAD_HELLO seq=%u payload_len=%u\n",
                   header->seq, header->payload_len);
        break;
    case AICP_SERVICE_CONTROL_APPLIED:
        if (service_stats.control_requests <= 16 ||
            (service_stats.control_requests % 100) == 0)
        {
            log_control(event->control, header->seq);
        }
        if ((service_stats.control_requests % 100) == 0)
        {
            log_runtime_stats("periodic");
        }
        break;
    case AICP_SERVICE_DUPLICATE:
        rt_kprintf("AICP_RTTHREAD_DUPLICATE seq=%u\n", header->seq);
        break;
    case AICP_SERVICE_STALE:
        rt_kprintf("AICP_RTTHREAD_STALE seq=%u\n", header->seq);
        break;
    case AICP_SERVICE_ERROR_SENT:
        rt_kprintf("AICP_RTTHREAD_ERROR_NOTIFY seq=%u code=%u\n",
                   header->seq, event->error_code);
        break;
    case AICP_SERVICE_DISCONNECTED:
        rt_kprintf("AICP_RTTHREAD_CLIENT_DONE ret=%d\n", event->result);
        break;
    case AICP_SERVICE_STATUS_SENT:
        break;
    }
}

static void serve_client(int fd)
{
    static struct aicp_service_session session;
    struct aicp_stream stream = {
        .read = rtthread_stream_read,
        .write = rtthread_stream_write,
        .context = &fd,
    };
    const struct aicp_service_ops ops = {
        .monotonic_ns = monotonic_ns,
        .on_event = log_service_event,
        .context = RT_NULL,
    };

    aicp_service_session_init(&session);
    (void)aicp_service_serve(&stream, &session, &service_stats, &ops);
}

int main(void)
{
    struct sockaddr_in address;
    int listen_fd;

    aicp_service_stats_init(&service_stats);
    wait_for_network();

    listen_fd = socket(AF_INET, SOCK_STREAM, 0);
    if (listen_fd < 0)
    {
        rt_kprintf("AICP_RTTHREAD_FATAL stage=socket errno=%d\n", errno);
        return -1;
    }

    rt_memset(&address, 0, sizeof(address));
    address.sin_family = AF_INET;
    address.sin_addr.s_addr = htonl(INADDR_ANY);
    address.sin_port = htons(AICP_PORT);
    if (bind(listen_fd, (struct sockaddr *)&address, sizeof(address)) != 0)
    {
        rt_kprintf("AICP_RTTHREAD_FATAL stage=bind errno=%d\n", errno);
        closesocket(listen_fd);
        return -1;
    }
    if (listen(listen_fd, 4) != 0)
    {
        rt_kprintf("AICP_RTTHREAD_FATAL stage=listen errno=%d\n", errno);
        closesocket(listen_fd);
        return -1;
    }

    rt_kprintf("AICP_RTTHREAD_READY transport=tcp port=%u\n", AICP_PORT);
    for (;;)
    {
        struct timeval timeout = { .tv_sec = 2, .tv_usec = 0 };
        int client_fd = accept(listen_fd, RT_NULL, RT_NULL);

        if (client_fd < 0)
        {
            rt_kprintf("AICP_RTTHREAD_ACCEPT_ERROR errno=%d\n", errno);
            continue;
        }
        setsockopt(client_fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout));
        setsockopt(client_fd, SOL_SOCKET, SO_SNDTIMEO, &timeout, sizeof(timeout));
        rt_kprintf("AICP_RTTHREAD_CLIENT_CONNECTED\n");
        ++accepted_clients;
        serve_client(client_fd);
        closesocket(client_fd);
        ++disconnected_clients;
        log_runtime_stats("disconnect");
    }
}
