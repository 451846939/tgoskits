// Copyright 2026 The TGOSKits Authors
//
// SPDX-License-Identifier: Apache-2.0

#define _POSIX_C_SOURCE 200809L

#include "aicp_client.h"
#include "aicp_posix_stream.h"
#include "control_policy.h"

#include <arpa/inet.h>
#include <math.h>
#include <netinet/in.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <time.h>

struct client_config {
    const char *host;
    uint16_t port;
    const char *csv_path;
    const char *mode;
    unsigned iterations;
    unsigned period_ms;
    unsigned reconnect_ms;
    unsigned io_timeout_ms;
    unsigned request_attempts;
};

static uint64_t monotonic_ns(void *context) {
    (void)context;
    struct timespec ts;
    if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0) {
        return 0;
    }
    return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
}

static void sleep_ms(unsigned ms) {
    struct timespec req = {
        .tv_sec = ms / 1000,
        .tv_nsec = (long)(ms % 1000) * 1000000L,
    };
    while (nanosleep(&req, &req) != 0) {
    }
}

static int set_io_timeout(int fd, unsigned timeout_ms) {
    struct timeval tv = {
        .tv_sec = (time_t)(timeout_ms / 1000),
        .tv_usec = (suseconds_t)((timeout_ms % 1000) * 1000),
    };
    if (setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof(tv)) != 0) {
        return -1;
    }
    if (setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &tv, sizeof(tv)) != 0) {
        return -1;
    }
    return 0;
}

static int connect_tcp(const char *host, uint16_t port, unsigned timeout_ms) {
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    if (fd < 0) {
        return -1;
    }
    if (set_io_timeout(fd, timeout_ms) != 0) {
        close(fd);
        return -1;
    }

    struct sockaddr_in addr;
    memset(&addr, 0, sizeof(addr));
    addr.sin_family = AF_INET;
    addr.sin_port = htons(port);
    if (inet_pton(AF_INET, host, &addr.sin_addr) != 1) {
        close(fd);
        return -1;
    }
    if (connect(fd, (struct sockaddr *)&addr, sizeof(addr)) != 0) {
        close(fd);
        return -1;
    }
    return fd;
}

static void usage(const char *argv0) {
    fprintf(stderr,
            "usage: %s [host] [port] [iterations] [csv] [ai|fixed]\n"
            "default: 127.0.0.1 8800 100 build/aicp_latency.csv ai\n",
            argv0);
}

int main(int argc, char **argv) {
    const struct aicp_client_ops client_ops = {
        .monotonic_ns = monotonic_ns,
        .on_event = NULL,
        .context = NULL,
    };
    const char hello[] =
        "{\"role\":\"linux-ai\",\"cap\":\"control,status,heartbeat\"}";
    struct client_config cfg = {
        .host = "127.0.0.1",
        .port = 8800,
        .csv_path = "build/aicp_latency.csv",
        .mode = "ai",
        .iterations = 100,
        .period_ms = 20,
        .reconnect_ms = 200,
        .io_timeout_ms = 1000,
        .request_attempts = 10,
    };
    if (argc > 6 || (argc > 1 && strcmp(argv[1], "-h") == 0)) {
        usage(argv[0]);
        return 2;
    }
    if (argc > 1) {
        cfg.host = argv[1];
    }
    if (argc > 2) {
        cfg.port = (uint16_t)strtoul(argv[2], NULL, 10);
    }
    if (argc > 3) {
        cfg.iterations = (unsigned)strtoul(argv[3], NULL, 10);
    }
    if (argc > 4) {
        cfg.csv_path = argv[4];
    }
    if (argc > 5) {
        cfg.mode = argv[5];
    }
    if (strcmp(cfg.mode, "ai") != 0 && strcmp(cfg.mode, "fixed") != 0) {
        usage(argv[0]);
        return 2;
    }

    FILE *csv = fopen(cfg.csv_path, "w");
    if (csv == NULL) {
        perror("fopen csv");
        return 1;
    }
    fprintf(csv, "seq,rtt_ns,target,measured,error,control_output\n");

    int fd = -1;
    uint32_t seq = 1;
    unsigned ok = 0;
    unsigned failed = 0;

    for (unsigned i = 0; i < cfg.iterations; i++) {
        const bool ai_mode = strcmp(cfg.mode, "ai") == 0;
        const struct aicp_control_payload control =
            aicp_control_policy(i, ai_mode);
        struct aicp_status_payload status;
        uint64_t rtt_ns = 0;
        int ret = -ETIMEDOUT;

        for (unsigned attempt = 1; attempt <= cfg.request_attempts; attempt++) {
            if (fd < 0) {
                fd = connect_tcp(cfg.host, cfg.port, cfg.io_timeout_ms);
                if (fd >= 0) {
                    struct aicp_posix_stream stream;
                    aicp_posix_stream_init(&stream, fd);
                    if (aicp_client_session_send_hello(
                            &stream.stream,
                            &seq,
                            hello,
                            sizeof(hello),
                            &client_ops) != 0) {
                        close(fd);
                        fd = -1;
                    }
                }
            }

            if (fd >= 0) {
                struct aicp_posix_stream stream;
                aicp_posix_stream_init(&stream, fd);
                ret = aicp_client_session_transact_control(
                    &stream.stream,
                    &seq,
                    &control,
                    &status,
                    &rtt_ns,
                    &client_ops);
                if (ret == 0) {
                    break;
                }
                close(fd);
                fd = -1;
            }

            if (attempt < cfg.request_attempts) {
                fprintf(stderr,
                        "AICP retry request=%u attempt=%u/%u ret=%d\n",
                        i + 1,
                        attempt,
                        cfg.request_attempts,
                        ret);
                sleep_ms(cfg.reconnect_ms);
            }
        }

        if (ret != 0) {
            failed++;
            continue;
        }

        ok++;
        printf("seq=%u target=%.3f measured=%.3f error=%.3f rtt=%llu ns\n",
               status.applied_seq,
               control.target,
               status.measured,
               status.error,
               (unsigned long long)rtt_ns);
        fprintf(csv,
                "%u,%llu,%.6f,%.6f,%.6f,%.6f\n",
                status.applied_seq,
                (unsigned long long)rtt_ns,
                control.target,
                status.measured,
                status.error,
                status.control_output);
        fflush(csv);
        sleep_ms(cfg.period_ms);
    }

    if (fd >= 0) {
        close(fd);
    }
    fclose(csv);
    fprintf(stderr, "AICP client complete: ok=%u failed=%u csv=%s\n", ok, failed, cfg.csv_path);
    return failed == 0 ? 0 : 1;
}
