// Copyright 2026 The TGOSKits Authors
//
// SPDX-License-Identifier: Apache-2.0

#include "aicp_service.h"

#include <errno.h>
#include <stdio.h>
#include <string.h>

#include <zephyr/kernel.h>
#include <zephyr/logging/log.h>
#include <zephyr/net/net_if.h>
#include <zephyr/net/net_ip.h>
#include <zephyr/net/socket.h>

LOG_MODULE_REGISTER(aicp_zephyr, LOG_LEVEL_INF);

#define AICP_PORT 8800

static ptrdiff_t zephyr_stream_read(void *context, void *buffer, size_t length)
{
    const int fd = *(const int *)context;
    const ssize_t result = zsock_recv(fd, buffer, length, 0);

    return result >= 0 ? (ptrdiff_t)result : (ptrdiff_t)-errno;
}

static ptrdiff_t zephyr_stream_write(
    void *context,
    const void *buffer,
    size_t length)
{
    const int fd = *(const int *)context;
    const ssize_t result = zsock_send(fd, buffer, length, 0);

    return result >= 0 ? (ptrdiff_t)result : (ptrdiff_t)-errno;
}

static uint64_t monotonic_ns(void *context) {
    (void)context;
    return k_uptime_get() * 1000000ull;
}

static int wait_for_network(void) {
    struct net_if *iface = net_if_get_default();
    struct net_if *configured_iface = NULL;
    struct in_addr configured_addr;
    char addr_buf[NET_IPV4_ADDR_LEN];

    if (iface == NULL) {
        LOG_ERR("AICP_ZEPHYR_NET_FAIL reason=no_default_interface");
        return -ENODEV;
    }
    if (net_addr_pton(AF_INET, CONFIG_NET_CONFIG_MY_IPV4_ADDR, &configured_addr) != 0) {
        LOG_ERR("AICP_ZEPHYR_NET_FAIL reason=invalid_ipv4 value=%s",
                CONFIG_NET_CONFIG_MY_IPV4_ADDR);
        return -EINVAL;
    }

    for (unsigned attempt = 0; attempt < 100; attempt++) {
        bool up = net_if_is_up(iface);
        bool has_addr = net_if_ipv4_addr_lookup(&configured_addr, &configured_iface) != NULL;

        if (up && has_addr && configured_iface == iface) {
            const struct net_linkaddr *link_addr = net_if_get_link_addr(iface);

            (void)net_addr_ntop(AF_INET, &configured_addr, addr_buf, sizeof(addr_buf));
            LOG_INF("AICP_ZEPHYR_NET_UP ifindex=%d ip=%s mac=%02x:%02x:%02x:%02x:%02x:%02x",
                    net_if_get_by_iface(iface),
                    addr_buf,
                    link_addr->addr[0], link_addr->addr[1], link_addr->addr[2],
                    link_addr->addr[3], link_addr->addr[4], link_addr->addr[5]);
            return 0;
        }
        k_sleep(K_MSEC(100));
    }

    LOG_ERR("AICP_ZEPHYR_NET_FAIL reason=timeout up=%d ip=%s",
            net_if_is_up(iface), CONFIG_NET_CONFIG_MY_IPV4_ADDR);
    return -ETIMEDOUT;
}

static int listen_tcp(uint16_t port) {
    int fd = zsock_socket(AF_INET, SOCK_STREAM, IPPROTO_TCP);
    if (fd < 0) {
        return -errno;
    }

    int enable = 1;
    (void)zsock_setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &enable, sizeof(enable));

    struct sockaddr_in addr;
    memset(&addr, 0, sizeof(addr));
    addr.sin_family = AF_INET;
    addr.sin_addr.s_addr = htonl(INADDR_ANY);
    addr.sin_port = htons(port);

    if (zsock_bind(fd, (struct sockaddr *)&addr, sizeof(addr)) != 0) {
        int err = -errno;
        zsock_close(fd);
        return err;
    }
    if (zsock_listen(fd, 2) != 0) {
        int err = -errno;
        zsock_close(fd);
        return err;
    }
    return fd;
}

static void log_service_event(
    void *context,
    const struct aicp_service_event_data *event) {
    (void)context;
    const struct aicp_header *header = event->header;

    switch (event->event) {
    case AICP_SERVICE_FRAME_RECEIVED:
        LOG_INF("AICP_ZEPHYR_RX_FRAME type=%u seq=%u len=%u",
                header->msg_type, header->seq, header->payload_len);
        break;
    case AICP_SERVICE_HELLO:
        LOG_INF("AICP_ZEPHYR_HELLO seq=%u payload_len=%u",
                header->seq, header->payload_len);
        break;
    case AICP_SERVICE_CONTROL_APPLIED:
        LOG_INF("AICP_ZEPHYR_CONTROL seq=%u target_milli=%d measured_milli=%d output_milli=%d mode=%u",
                header->seq,
                (int)(event->control->setpoint * 1000.0f),
                (int)(event->control->measured * 1000.0f),
                (int)(event->control->control_output * 1000.0f),
                event->control->mode);
        break;
    case AICP_SERVICE_DUPLICATE:
        LOG_WRN("AICP_ZEPHYR_DUPLICATE seq=%u", header->seq);
        break;
    case AICP_SERVICE_STALE:
        LOG_WRN("AICP_ZEPHYR_STALE seq=%u", header->seq);
        break;
    case AICP_SERVICE_ERROR_SENT:
        LOG_WRN("AICP_ZEPHYR_ERROR_NOTIFY seq=%u code=%u",
                header->seq, event->error_code);
        break;
    case AICP_SERVICE_DISCONNECTED:
        LOG_INF("AICP_ZEPHYR_CLIENT_DISCONNECTED ret=%d", event->result);
        break;
    case AICP_SERVICE_STATUS_SENT:
        break;
    }
}

static void serve_client(int fd, struct aicp_service_stats *stats) {
    static struct aicp_service_session session;
    struct aicp_stream stream = {
        .read = zephyr_stream_read,
        .write = zephyr_stream_write,
        .context = &fd,
    };
    const struct aicp_service_ops ops = {
        .monotonic_ns = monotonic_ns,
        .on_event = log_service_event,
        .context = NULL,
    };

    aicp_service_session_init(&session);
    (void)aicp_service_serve(&stream, &session, stats, &ops);
    zsock_close(fd);
}

int main(void) {
    int ret = wait_for_network();
    if (ret != 0) {
        return 1;
    }

    int listen_fd = listen_tcp(AICP_PORT);
    if (listen_fd < 0) {
        LOG_ERR("AICP listen failed ret=%d", listen_fd);
        return 1;
    }

    LOG_INF("AICP Zephyr RTOS server listening on 0.0.0.0:%u", AICP_PORT);
    struct aicp_service_stats stats;
    aicp_service_stats_init(&stats);
    for (;;) {
        int fd = zsock_accept(listen_fd, NULL, NULL);
        if (fd < 0) {
            LOG_ERR("accept failed errno=%d", errno);
            k_sleep(K_MSEC(100));
            continue;
        }
        serve_client(fd, &stats);
    }
}
