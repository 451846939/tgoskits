// Copyright 2026 The TGOSKits Authors
//
// SPDX-License-Identifier: Apache-2.0

#define _POSIX_C_SOURCE 200809L

#include "aicp_posix_stream.h"
#include "aicp_service.h"

#include <arpa/inet.h>
#include <netinet/in.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <time.h>

static uint64_t monotonic_ns(void) {
    struct timespec ts;
    if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0) {
        return 0;
    }
    return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
}

static int listen_tcp(uint16_t port) {
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    if (fd < 0) {
        return -1;
    }

    int enable = 1;
    setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &enable, sizeof(enable));

    struct sockaddr_in addr;
    memset(&addr, 0, sizeof(addr));
    addr.sin_family = AF_INET;
    addr.sin_addr.s_addr = htonl(INADDR_ANY);
    addr.sin_port = htons(port);
    if (bind(fd, (struct sockaddr *)&addr, sizeof(addr)) != 0) {
        close(fd);
        return -1;
    }
    if (listen(fd, 4) != 0) {
        close(fd);
        return -1;
    }
    return fd;
}

static void set_client_timeout(int fd) {
    struct timeval tv = {
        .tv_sec = 2,
        .tv_usec = 0,
    };
    setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof(tv));
    setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &tv, sizeof(tv));
}

static uint64_t service_monotonic_ns(void *context) {
    (void)context;
    return monotonic_ns();
}

static void log_service_event(
    void *context,
    const struct aicp_service_event_data *event) {
    (void)context;
    const struct aicp_header *header = event->header;

    switch (event->event) {
    case AICP_SERVICE_HELLO:
        printf("AICP HELLO seq=%u payload_len=%u\n", header->seq, header->payload_len);
        break;
    case AICP_SERVICE_CONTROL_APPLIED:
        printf("CONTROL seq=%u target=%.3f measured=%.3f output=%.3f\n",
               header->seq,
               event->control->setpoint,
               event->control->measured,
               event->control->control_output);
        break;
    case AICP_SERVICE_DUPLICATE:
        printf("AICP DUPLICATE seq=%u\n", header->seq);
        break;
    case AICP_SERVICE_STALE:
        printf("AICP STALE seq=%u\n", header->seq);
        break;
    case AICP_SERVICE_DISCONNECTED:
        printf("AICP CLIENT_DONE ret=%d\n", event->result);
        break;
    case AICP_SERVICE_FRAME_RECEIVED:
    case AICP_SERVICE_STATUS_SENT:
    case AICP_SERVICE_ERROR_SENT:
        break;
    }
}

static void serve_client(int fd, struct aicp_service_stats *stats) {
    struct aicp_service_session session;
    struct aicp_posix_stream stream;
    const struct aicp_service_ops ops = {
        .monotonic_ns = service_monotonic_ns,
        .on_event = log_service_event,
        .context = NULL,
    };

    aicp_service_session_init(&session);
    aicp_posix_stream_init(&stream, fd);
    (void)aicp_service_serve(&stream.stream, &session, stats, &ops);
    close(fd);
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IOLBF, 0);

    uint16_t port = 8800;
    if (argc > 1) {
        port = (uint16_t)strtoul(argv[1], NULL, 10);
    }

    int listen_fd = listen_tcp(port);
    if (listen_fd < 0) {
        perror("listen_tcp");
        return 1;
    }
    printf("AICP RTOS reference server listening on 0.0.0.0:%u\n", port);

    struct aicp_service_stats stats;
    aicp_service_stats_init(&stats);

    for (;;) {
        int fd = accept(listen_fd, NULL, NULL);
        if (fd < 0) {
            perror("accept");
            continue;
        }
        set_client_timeout(fd);
        serve_client(fd, &stats);
    }
}
