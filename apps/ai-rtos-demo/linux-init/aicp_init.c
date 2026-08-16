// Copyright 2026 The TGOSKits Authors
//
// SPDX-License-Identifier: Apache-2.0

#define _GNU_SOURCE

#include "aicp_client.h"
#include "aicp_datagram.h"
#include "aicp_posix_stream.h"
#include "control_policy.h"

#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <math.h>
#include <net/if_arp.h>
#include <net/if.h>
#include <netinet/in.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mount.h>
#include <sys/reboot.h>
#include <sys/stat.h>
#include <net/route.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/types.h>
#include <time.h>
#include <unistd.h>

#ifndef AICP_INIT_GUEST_LABEL
#define AICP_INIT_GUEST_LABEL "Linux guest"
#endif

#ifndef AICP_INIT_ROLE
#define AICP_INIT_ROLE "linux-guest-init"
#endif

#ifndef AICP_INIT_DONE_TOKEN
#define AICP_INIT_DONE_TOKEN "AICP_LINUX_DONE"
#endif

#ifndef AICP_INIT_STATUS_TOKEN
#define AICP_INIT_STATUS_TOKEN "AICP_LINUX_STATUS"
#endif

#ifndef AICP_INIT_FILE_TOKEN
#define AICP_INIT_FILE_TOKEN "AICP_LINUX_FILE"
#endif

#ifndef AICP_INIT_NETDIAG_TOKEN
#define AICP_INIT_NETDIAG_TOKEN "AICP_LINUX_NETDIAG"
#endif

#ifndef AICP_INIT_STRESS_TOKEN
#define AICP_INIT_STRESS_TOKEN "AICP_LINUX_STRESS"
#endif

#ifndef AICP_INIT_NET_PREFIX
#define AICP_INIT_NET_PREFIX "10.0.3.0"
#endif

#ifndef AICP_INIT_SERVER
#define AICP_INIT_SERVER "10.0.3.2"
#endif

#ifndef AICP_INIT_SERVER_PORT
#define AICP_INIT_SERVER_PORT 8800u
#endif

#ifndef AICP_INIT_CLIENT
#define AICP_INIT_CLIENT "10.0.3.3"
#endif

#ifndef AICP_INIT_NETMASK
#define AICP_INIT_NETMASK "255.255.255.0"
#endif

#ifndef AICP_INIT_SERVER_MAC
#define AICP_INIT_SERVER_MAC "52:54:00:aa:03:02"
#endif

#ifndef AICP_INIT_IFACE
#define AICP_INIT_IFACE "eth0"
#endif

#ifndef AICP_INIT_STATIC_ARP
#define AICP_INIT_STATIC_ARP 1
#endif

#ifndef AICP_INIT_ITERATIONS
#define AICP_INIT_ITERATIONS 40u
#endif

#ifndef AICP_INIT_MODE
#define AICP_INIT_MODE "ai"
#endif

#ifndef AICP_INIT_STRESS_PROCS
#define AICP_INIT_STRESS_PROCS 0u
#endif

#ifndef AICP_INIT_CONNECT_RETRIES
#define AICP_INIT_CONNECT_RETRIES 120u
#endif

#ifndef AICP_INIT_TCP_TIMEOUT_MS
#define AICP_INIT_TCP_TIMEOUT_MS 3000u
#endif

#ifndef AICP_INIT_LINK_TIMEOUT_MS
#define AICP_INIT_LINK_TIMEOUT_MS 30000u
#endif

#ifndef AICP_INIT_TRANSPORT
#define AICP_INIT_TRANSPORT "tcp"
#endif

#ifndef AICP_INIT_UDP_RETRIES
#define AICP_INIT_UDP_RETRIES 8u
#endif

#ifndef AICP_INIT_UDP_REORDER_TEST
#define AICP_INIT_UDP_REORDER_TEST 0
#endif

#ifndef AICP_INIT_RAW_SYSCALL_IO
#define AICP_INIT_RAW_SYSCALL_IO 1
#endif

#ifndef AICP_INIT_RELIABILITY_TEST
#define AICP_INIT_RELIABILITY_TEST 0
#endif

static uint64_t monotonic_ns(void) {
    struct timespec ts;
    if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0) {
        return 0;
    }
    return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
}

static uint64_t client_monotonic_ns(void *context) {
    (void)context;
    return monotonic_ns();
}

struct bounded_posix_stream {
    struct aicp_stream stream;
    int fd;
    uint64_t io_deadline_ns;
};

static int bounded_stream_retry(struct bounded_posix_stream *stream) {
    if (monotonic_ns() >= stream->io_deadline_ns) {
        return -ETIMEDOUT;
    }
    if (sched_yield() != 0) {
        return -errno;
    }
    return 0;
}

static ptrdiff_t bounded_stream_read(void *context, void *buffer, size_t length) {
    struct bounded_posix_stream *stream = context;
    for (;;) {
        const ssize_t result = recv(stream->fd, buffer, length, MSG_DONTWAIT);
        if (result >= 0) {
            return (ptrdiff_t)result;
        }
        if (errno == EINTR) {
            continue;
        }
        if (errno != EAGAIN && errno != EWOULDBLOCK) {
            return (ptrdiff_t)-errno;
        }
        const int retry = bounded_stream_retry(stream);
        if (retry != 0) {
            return retry;
        }
    }
}

static ptrdiff_t bounded_stream_write(void *context, const void *buffer, size_t length) {
    struct bounded_posix_stream *stream = context;
    for (;;) {
        const ssize_t result = send(stream->fd, buffer, length, MSG_DONTWAIT | MSG_NOSIGNAL);
        if (result >= 0) {
            return (ptrdiff_t)result;
        }
        if (errno == EINTR) {
            continue;
        }
        if (errno != EAGAIN && errno != EWOULDBLOCK) {
            return (ptrdiff_t)-errno;
        }
        const int retry = bounded_stream_retry(stream);
        if (retry != 0) {
            return retry;
        }
    }
}

static void bounded_stream_init(
    struct bounded_posix_stream *stream,
    int fd,
    unsigned timeout_ms) {
    const uint64_t now_ns = monotonic_ns();
    const uint64_t timeout_ns = (uint64_t)timeout_ms * 1000000ull;

    stream->fd = fd;
    stream->io_deadline_ns = now_ns > UINT64_MAX - timeout_ns
                                 ? UINT64_MAX
                                 : now_ns + timeout_ns;
    stream->stream.read = bounded_stream_read;
    stream->stream.write = bounded_stream_write;
    stream->stream.context = stream;
}

static void sleep_ms(unsigned ms) {
    struct timespec req = {
        .tv_sec = ms / 1000,
        .tv_nsec = (long)(ms % 1000) * 1000000L,
    };
    while (nanosleep(&req, &req) != 0 && errno == EINTR) {
    }
}

#if AICP_INIT_STRESS_PROCS > 0
static void stress_worker(unsigned worker) {
    volatile uint64_t x = 0x9e3779b97f4a7c15ull ^ worker;
    uint8_t *buf = malloc(1024 * 1024);
    if (buf == NULL) {
        printf("%s worker=%u malloc_failed errno=%d\n", AICP_INIT_STRESS_TOKEN, worker, errno);
    }

    for (;;) {
        for (unsigned i = 0; i < 4096; i++) {
            x ^= x << 7;
            x ^= x >> 9;
            x += 0x100000001b3ull + i + worker;
            if (buf != NULL) {
                buf[(i * 257u) & ((1024u * 1024u) - 1u)] = (uint8_t)x;
            }
        }
        if ((x & 0xfffffu) == 0) {
            sleep_ms(1);
        }
    }
}

static void start_stress_procs(void) {
    for (unsigned i = 0; i < AICP_INIT_STRESS_PROCS; i++) {
        pid_t pid = fork();
        if (pid == 0) {
            stress_worker(i);
            _exit(0);
        }
        if (pid < 0) {
            printf("%s worker=%u fork_failed errno=%d\n", AICP_INIT_STRESS_TOKEN, i, errno);
        } else {
            printf("%s worker=%u pid=%d started\n", AICP_INIT_STRESS_TOKEN, i, (int)pid);
        }
    }
}
#else
static void start_stress_procs(void) {
}
#endif

#define AICP_CPU_STAT_MAX 64u

struct cpu_stat_sample {
    uint64_t total;
    uint64_t idle;
    int valid;
};

static void read_cpu_stats(struct cpu_stat_sample stats[AICP_CPU_STAT_MAX]) {
    FILE *file = fopen("/proc/stat", "r");
    char line[512];

    memset(stats, 0, sizeof(struct cpu_stat_sample) * AICP_CPU_STAT_MAX);
    if (file == NULL) {
        printf("AICP_LINUX_CPU read_failed errno=%d\n", errno);
        return;
    }
    while (fgets(line, sizeof(line), file) != NULL) {
        unsigned cpu;
        unsigned long long user, nice, system, idle, iowait, irq, softirq, steal;
        int fields = sscanf(line, "cpu%u %llu %llu %llu %llu %llu %llu %llu %llu",
                            &cpu, &user, &nice, &system, &idle, &iowait, &irq,
                            &softirq, &steal);
        if (fields != 9 || cpu >= AICP_CPU_STAT_MAX) {
            continue;
        }
        stats[cpu].idle = idle + iowait;
        stats[cpu].total = user + nice + system + idle + iowait + irq + softirq + steal;
        stats[cpu].valid = 1;
    }
    fclose(file);
}

static void print_cpu_usage(const struct cpu_stat_sample before[AICP_CPU_STAT_MAX],
                            const struct cpu_stat_sample after[AICP_CPU_STAT_MAX]) {
    for (unsigned cpu = 0; cpu < AICP_CPU_STAT_MAX; cpu++) {
        if (!before[cpu].valid || !after[cpu].valid || after[cpu].total <= before[cpu].total) {
            continue;
        }
        uint64_t total = after[cpu].total - before[cpu].total;
        uint64_t idle = after[cpu].idle - before[cpu].idle;
        uint64_t busy = total > idle ? total - idle : 0;
        printf("AICP_LINUX_CPU cpu=%u busy_ticks=%llu total_ticks=%llu usage_permille=%llu\n",
               cpu, (unsigned long long)busy, (unsigned long long)total,
               (unsigned long long)(busy * 1000ull / total));
    }
}

static void ensure_virtual_fs(void) {
    (void)mkdir("/proc", 0555);
    (void)mkdir("/sys", 0555);
    if (mount("proc", "/proc", "proc", 0, "") != 0 && errno != EBUSY) {
        printf("AICP %s mount /proc failed errno=%d\n", AICP_INIT_GUEST_LABEL, errno);
    }
    if (mount("sysfs", "/sys", "sysfs", 0, "") != 0 && errno != EBUSY) {
        printf("AICP %s mount /sys failed errno=%d\n", AICP_INIT_GUEST_LABEL, errno);
    }
}

static int set_ifaddr(int ctl, const char *ifname, unsigned long request, const char *addr) {
    struct ifreq ifr;
    struct sockaddr_in *sin = (struct sockaddr_in *)&ifr.ifr_addr;

    memset(&ifr, 0, sizeof(ifr));
    snprintf(ifr.ifr_name, sizeof(ifr.ifr_name), "%s", ifname);
    sin->sin_family = AF_INET;
    if (inet_pton(AF_INET, addr, &sin->sin_addr) != 1) {
        return -EINVAL;
    }
    if (ioctl(ctl, request, &ifr) != 0) {
        return -errno;
    }
    return 0;
}

static int set_if_up(int ctl, const char *ifname) {
    struct ifreq ifr;

    memset(&ifr, 0, sizeof(ifr));
    snprintf(ifr.ifr_name, sizeof(ifr.ifr_name), "%s", ifname);
    if (ioctl(ctl, SIOCGIFFLAGS, &ifr) != 0) {
        return -errno;
    }
    ifr.ifr_flags |= IFF_UP;
    if (ioctl(ctl, SIOCSIFFLAGS, &ifr) != 0) {
        return -errno;
    }
    return 0;
}

static int wait_if_running(int ctl, const char *ifname, unsigned timeout_ms) {
    const uint64_t deadline = monotonic_ns() + (uint64_t)timeout_ms * 1000000ull;
    for (;;) {
        struct ifreq ifr;
        memset(&ifr, 0, sizeof(ifr));
        snprintf(ifr.ifr_name, sizeof(ifr.ifr_name), "%s", ifname);
        if (ioctl(ctl, SIOCGIFFLAGS, &ifr) != 0) {
            return -errno;
        }
        if ((ifr.ifr_flags & IFF_RUNNING) != 0) {
            return 0;
        }
        if (monotonic_ns() >= deadline) {
            return -ETIMEDOUT;
        }
        sleep_ms(10u);
    }
}

static int parse_mac(const char *text, unsigned char out[6]) {
    unsigned values[6];
    if (sscanf(text,
               "%x:%x:%x:%x:%x:%x",
               &values[0],
               &values[1],
               &values[2],
               &values[3],
               &values[4],
               &values[5]) != 6) {
        return -EINVAL;
    }
    for (unsigned i = 0; i < 6; i++) {
        if (values[i] > 0xffu) {
            return -EINVAL;
        }
        out[i] = (unsigned char)values[i];
    }
    return 0;
}

static int add_connected_route(int ctl) {
    struct rtentry route;
    struct sockaddr_in *dst = (struct sockaddr_in *)&route.rt_dst;
    struct sockaddr_in *mask = (struct sockaddr_in *)&route.rt_genmask;

    memset(&route, 0, sizeof(route));
    dst->sin_family = AF_INET;
    mask->sin_family = AF_INET;
    if (inet_pton(AF_INET, AICP_INIT_NET_PREFIX, &dst->sin_addr) != 1 ||
        inet_pton(AF_INET, AICP_INIT_NETMASK, &mask->sin_addr) != 1) {
        return -EINVAL;
    }
    route.rt_flags = RTF_UP;
    route.rt_dev = (char *)AICP_INIT_IFACE;
    if (ioctl(ctl, SIOCADDRT, &route) != 0 && errno != EEXIST) {
        return -errno;
    }
    return 0;
}

static int add_static_arp(int ctl) {
    struct arpreq req;
    struct sockaddr_in *pa = (struct sockaddr_in *)&req.arp_pa;
    unsigned char mac[6];

    int ret = parse_mac(AICP_INIT_SERVER_MAC, mac);
    if (ret != 0) {
        return ret;
    }

    memset(&req, 0, sizeof(req));
    pa->sin_family = AF_INET;
    if (inet_pton(AF_INET, AICP_INIT_SERVER, &pa->sin_addr) != 1) {
        return -EINVAL;
    }
    req.arp_ha.sa_family = ARPHRD_ETHER;
    memcpy(req.arp_ha.sa_data, mac, sizeof(mac));
    req.arp_flags = ATF_COM | ATF_PERM;
    snprintf(req.arp_dev, sizeof(req.arp_dev), "%s", AICP_INIT_IFACE);
    if (ioctl(ctl, SIOCSARP, &req) != 0) {
        return -errno;
    }
    return 0;
}

static int configure_iface(void) {
    int ctl = socket(AF_INET, SOCK_DGRAM, 0);
    if (ctl < 0) {
        return -errno;
    }

    int ret = set_ifaddr(ctl, AICP_INIT_IFACE, SIOCSIFADDR, AICP_INIT_CLIENT);
    printf("AICP %s netcfg step=SIOCSIFADDR ret=%d\n", AICP_INIT_GUEST_LABEL, ret);
    if (ret == 0) {
        ret = set_ifaddr(ctl, AICP_INIT_IFACE, SIOCSIFNETMASK, AICP_INIT_NETMASK);
        printf("AICP %s netcfg step=SIOCSIFNETMASK ret=%d\n", AICP_INIT_GUEST_LABEL, ret);
    }
    if (ret == 0) {
        ret = set_if_up(ctl, AICP_INIT_IFACE);
        printf("AICP %s netcfg step=SIOCSIFFLAGS ret=%d\n", AICP_INIT_GUEST_LABEL, ret);
    }
    if (ret == 0) {
        ret = wait_if_running(ctl, AICP_INIT_IFACE, AICP_INIT_LINK_TIMEOUT_MS);
        printf("AICP %s netcfg step=WAIT_RUNNING ret=%d timeout_ms=%u\n",
               AICP_INIT_GUEST_LABEL,
               ret,
               AICP_INIT_LINK_TIMEOUT_MS);
    }
    if (ret == 0) {
        ret = add_connected_route(ctl);
        printf("AICP %s netcfg step=SIOCADDRT ret=%d\n", AICP_INIT_GUEST_LABEL, ret);
    }
    if (ret == 0 && AICP_INIT_STATIC_ARP) {
        ret = add_static_arp(ctl);
        printf("AICP %s netcfg step=SIOCSARP ret=%d server=%s mac=%s\n",
               AICP_INIT_GUEST_LABEL,
               ret,
               AICP_INIT_SERVER,
               AICP_INIT_SERVER_MAC);
    }

    close(ctl);
    return ret;
}

static unsigned long long read_stat(const char *name) {
    char path[128];
    char buf[64];
    snprintf(path, sizeof(path), "/sys/class/net/%s/statistics/%s", AICP_INIT_IFACE, name);
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
        return ULLONG_MAX;
    }
    ssize_t len = read(fd, buf, sizeof(buf) - 1);
    close(fd);
    if (len <= 0) {
        return 0;
    }
    buf[len] = '\0';
    return strtoull(buf, NULL, 10);
}

static void dump_small_file(const char *tag, const char *path) {
    char buf[1024];
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
        printf("%s tag=%s path=%s open_errno=%d\n", AICP_INIT_FILE_TOKEN, tag, path, errno);
        return;
    }
    ssize_t len = read(fd, buf, sizeof(buf) - 1);
    close(fd);
    if (len <= 0) {
        printf("%s tag=%s path=%s read_ret=%zd errno=%d\n", AICP_INIT_FILE_TOKEN, tag, path, len, errno);
        return;
    }
    buf[len] = '\0';
    for (ssize_t i = 0; i < len; i++) {
        if (buf[i] == '\n' || buf[i] == '\t') {
            buf[i] = ' ';
        }
    }
    printf("%s tag=%s path=%s data=%s\n", AICP_INIT_FILE_TOKEN, tag, path, buf);
}

static void dump_irq_diagnostics(const char *tag, int include_affinity) {
    FILE *interrupts = fopen("/proc/interrupts", "re");
    if (interrupts == NULL) {
        printf("%s tag=%s path=/proc/interrupts open_errno=%d\n",
               AICP_INIT_FILE_TOKEN,
               tag,
               errno);
        return;
    }

    char line[1024];
    while (fgets(line, sizeof(line), interrupts) != NULL) {
        if (strstr(line, "virtio") == NULL && strstr(line, AICP_INIT_IFACE) == NULL) {
            continue;
        }

        char *cursor = line;
        while (*cursor == ' ' || *cursor == '\t') {
            cursor++;
        }
        errno = 0;
        char *end = NULL;
        unsigned long irq = strtoul(cursor, &end, 10);
        if (errno != 0 || end == cursor || *end != ':') {
            continue;
        }

        for (char *ch = line; *ch != '\0'; ch++) {
            if (*ch == '\n' || *ch == '\t') {
                *ch = ' ';
            }
        }
        printf("%s tag=%s irq=%lu line=%s\n", AICP_INIT_NETDIAG_TOKEN, tag, irq, line);

        if (include_affinity) {
            char path[PATH_MAX];
            snprintf(path, sizeof(path), "/proc/irq/%lu/smp_affinity_list", irq);
            dump_small_file(tag, path);
            snprintf(path, sizeof(path), "/proc/irq/%lu/effective_affinity_list", irq);
            dump_small_file(tag, path);
        }
    }
    fclose(interrupts);
}

static void dump_iface_diag(const char *tag) {
    int ctl = socket(AF_INET, SOCK_DGRAM, 0);
    struct ifreq ifr;
    unsigned flags = 0;
    unsigned char mac[6] = { 0 };

    if (ctl >= 0) {
        memset(&ifr, 0, sizeof(ifr));
        snprintf(ifr.ifr_name, sizeof(ifr.ifr_name), "%s", AICP_INIT_IFACE);
        if (ioctl(ctl, SIOCGIFFLAGS, &ifr) == 0) {
            flags = (unsigned)ifr.ifr_flags;
        }
        memset(&ifr, 0, sizeof(ifr));
        snprintf(ifr.ifr_name, sizeof(ifr.ifr_name), "%s", AICP_INIT_IFACE);
        if (ioctl(ctl, SIOCGIFHWADDR, &ifr) == 0) {
            memcpy(mac, ifr.ifr_hwaddr.sa_data, sizeof(mac));
        }
        close(ctl);
    }

    printf("%s tag=%s flags=0x%x mac=%02x:%02x:%02x:%02x:%02x:%02x "
           "tx_packets=%llu rx_packets=%llu tx_bytes=%llu rx_bytes=%llu "
           "tx_errors=%llu rx_errors=%llu\n",
           AICP_INIT_NETDIAG_TOKEN,
           tag,
           flags,
           mac[0],
           mac[1],
           mac[2],
           mac[3],
           mac[4],
           mac[5],
           read_stat("tx_packets"),
           read_stat("rx_packets"),
           read_stat("tx_bytes"),
           read_stat("rx_bytes"),
           read_stat("tx_errors"),
           read_stat("rx_errors"));
    dump_small_file(tag, "/proc/net/route");
    dump_small_file(tag, "/proc/net/arp");
    // Affinity is configured once and already captured before the workload.
    // Re-reading the per-IRQ procfs files while Linux is still completing SMP
    // startup can wait on hotplug locks and hide an otherwise successful run.
    dump_irq_diagnostics(tag, strcmp(tag, "configured") == 0);
}

static int set_io_timeout(int fd, unsigned timeout_ms, const char **stage) {
    struct timeval tv = {
        .tv_sec = (time_t)(timeout_ms / 1000),
        .tv_usec = (suseconds_t)((timeout_ms % 1000) * 1000),
    };
    if (setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof(tv)) != 0) {
        if (stage != NULL) {
            *stage = "setsockopt-rcvtimeo";
        }
        return -errno;
    }
    if (setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &tv, sizeof(tv)) != 0) {
        if (stage != NULL) {
            *stage = "setsockopt-sndtimeo";
        }
        return -errno;
    }
    return 0;
}

static int connect_tcp(const char *host, uint16_t port, unsigned timeout_ms, const char **stage) {
    if (stage != NULL) {
        *stage = "socket";
    }
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    if (fd < 0) {
        return -errno;
    }
    printf("AICP %s connect_trace step=socket fd=%d\n", AICP_INIT_GUEST_LABEL, fd);
    if (stage != NULL) {
        *stage = "setsockopt";
    }
    int ret = set_io_timeout(fd, timeout_ms, stage);
    if (ret != 0) {
        printf("AICP %s connect_trace step=setsockopt ret=%d\n", AICP_INIT_GUEST_LABEL, ret);
        close(fd);
        return ret;
    }
    printf("AICP %s connect_trace step=setsockopt ret=0 timeout_ms=%u\n",
           AICP_INIT_GUEST_LABEL,
           timeout_ms);

    struct sockaddr_in addr;
    memset(&addr, 0, sizeof(addr));
    addr.sin_family = AF_INET;
    addr.sin_port = htons(port);
    if (inet_pton(AF_INET, host, &addr.sin_addr) != 1) {
        if (stage != NULL) {
            *stage = "inet_pton";
        }
        close(fd);
        return -EINVAL;
    }
    if (stage != NULL) {
        *stage = "connect";
    }
    printf("AICP %s connect_trace step=connect_begin target=%s:%u\n",
           AICP_INIT_GUEST_LABEL,
           host,
           port);
    printf("AICP %s connect_trace step=connect_call sysno=%ld\n",
           AICP_INIT_GUEST_LABEL,
           (long)SYS_connect);
    if (connect(fd, (struct sockaddr *)&addr, sizeof(addr)) != 0) {
        ret = -errno;
        printf("AICP %s connect_trace step=connect_end ret=%d\n", AICP_INIT_GUEST_LABEL, ret);
        close(fd);
        return ret;
    }
    printf("AICP %s connect_trace step=connect_end ret=0\n", AICP_INIT_GUEST_LABEL);
    if (stage != NULL) {
        *stage = "connected";
    }
    return fd;
}

static struct aicp_header make_header(
    uint8_t msg_type,
    uint16_t flags,
    uint32_t payload_len,
    uint32_t seq) {
    return aicp_make_header(msg_type, flags, payload_len, seq, monotonic_ns(), AICP_OK);
}

static void trace_client_event(
    void *context,
    const struct aicp_client_event *event) {
    (void)context;
    const struct aicp_header *request = event->request;

    if (event->kind == AICP_CLIENT_TX_BEGIN && request != NULL) {
        const char *type = request->msg_type == AICP_MSG_HELLO ? "HELLO" : "CONTROL_SET";
        printf("AICP_LINUX_TX_FRAME type=%s seq=%u len=%u\n",
               type,
               request->seq,
               request->payload_len);
        return;
    }
    if (event->kind == AICP_CLIENT_TX_COMPLETE && request != NULL &&
        (request->msg_type == AICP_MSG_HELLO || event->result != 0)) {
        const char *type = request->msg_type == AICP_MSG_HELLO ? "HELLO" : "CONTROL_SET";
        printf("AICP_LINUX_TX_RESULT type=%s seq=%u ret=%d\n",
               type,
               request->seq,
               event->result);
        return;
    }
    if (event->kind != AICP_CLIENT_RX_COMPLETE || request == NULL) {
        return;
    }
    if (event->result != 0 || event->response == NULL) {
        printf("AICP_LINUX_RX_RESULT expected=STATUS seq=%u ret=%d\n",
               request->seq,
               event->result);
        return;
    }

    printf("AICP_LINUX_RX_FRAME type=%u seq=%u len=%u error=%u\n",
           event->response->msg_type,
           event->response->seq,
           event->response->payload_len,
           event->response->error_code);
}

static const struct aicp_client_ops client_ops = {
    .monotonic_ns = client_monotonic_ns,
    .on_event = trace_client_event,
    .context = NULL,
};

static int send_hello(int fd, uint32_t *seq) {
    char payload[96];
    struct bounded_posix_stream stream;

    bounded_stream_init(&stream, fd, AICP_INIT_TCP_TIMEOUT_MS);
    snprintf(payload,
             sizeof(payload),
             "{\"role\":\"%s\",\"cap\":\"ai,control,status\"}",
             AICP_INIT_ROLE);
    return aicp_client_session_send_hello(
        &stream.stream,
        seq,
        payload,
        (uint32_t)strlen(payload) + 1u,
        &client_ops);
}

static int transact_control(
    int fd,
    uint32_t *seq,
    const struct aicp_control_payload *control,
    struct aicp_status_payload *status,
    uint64_t *rtt_ns) {
    struct bounded_posix_stream stream;

    bounded_stream_init(&stream, fd, AICP_INIT_TCP_TIMEOUT_MS);
    return aicp_client_session_transact_control(
        &stream.stream, seq, control, status, rtt_ns, &client_ops);
}

#if AICP_INIT_RELIABILITY_TEST
static int send_raw_frame(int fd, struct aicp_header header, const void *payload) {
    uint8_t wire[AICP_HEADER_LEN];
    struct aicp_posix_stream stream;

    aicp_posix_stream_init(&stream, fd);

    header.magic = AICP_MAGIC;
    header.header_len = AICP_HEADER_LEN;
    header.crc16 = aicp_frame_crc(header, payload);
    aicp_header_encode(&header, wire);

    int ret = aicp_stream_write_full(&stream.stream, wire, sizeof(wire));
    if (ret != 0) {
        return ret;
    }
    if (header.payload_len != 0) {
        return aicp_stream_write_full(&stream.stream, payload, header.payload_len);
    }
    return 0;
}

static int expect_reply(
    int fd,
    uint8_t expected_type,
    uint32_t expected_seq,
    uint16_t expected_error,
    struct aicp_status_payload *status) {
    uint8_t payload[AICP_MAX_PAYLOAD];
    struct aicp_header header;
    struct aicp_posix_stream stream;

    aicp_posix_stream_init(&stream, fd);
    int ret = aicp_stream_recv_frame(
        &stream.stream, &header, payload, sizeof(payload));

    if (ret != 0) {
        return ret;
    }
    if (header.msg_type != expected_type || header.seq != expected_seq ||
        header.error_code != expected_error) {
        return -EPROTO;
    }
    if (expected_type == AICP_MSG_STATUS) {
        if (header.payload_len != sizeof(*status) || status == NULL) {
            return -EPROTO;
        }
        memcpy(status, payload, sizeof(*status));
    }
    return 0;
}

static int send_frame(
    int fd,
    struct aicp_header header,
    const void *payload) {
    struct aicp_posix_stream stream;

    aicp_posix_stream_init(&stream, fd);
    return aicp_stream_send_frame(&stream.stream, header, payload);
}

static int report_reliability_case(const char *name, int ret, unsigned *passed) {
    printf("AICP_RTTHREAD_RELIABILITY name=%s result=%s ret=%d\n",
           name,
           ret == 0 ? "PASS" : "FAIL",
           ret);
    if (ret == 0) {
        (*passed)++;
    }
    return ret;
}

static int connect_reliability_client(void) {
    for (unsigned retry = 0; retry < AICP_INIT_CONNECT_RETRIES; retry++) {
        const char *stage = "start";
        int fd = connect_tcp(
            AICP_INIT_SERVER,
            (uint16_t)AICP_INIT_SERVER_PORT,
            AICP_INIT_TCP_TIMEOUT_MS,
            &stage);
        if (fd >= 0) {
            return fd;
        }
        printf("AICP_RTTHREAD_RELIABILITY connect_retry=%u stage=%s ret=%d\n",
               retry + 1,
               stage,
               fd);
        sleep_ms(200);
    }
    return -ETIMEDOUT;
}

static int run_rtthread_reliability_tests(void) {
    static const unsigned expected_cases = 8;
    struct aicp_status_payload control_status;
    struct aicp_status_payload duplicate_status;
    struct aicp_status_payload heartbeat_status;
    struct aicp_control_payload control = {
        .target = 0.75f,
        .kp = 0.64f,
        .ki = 0.08f,
        .kd = 0.02f,
        .feed_forward = 0.05f,
        .mode = 1u,
    };
    uint8_t bad_payload[4] = {0xde, 0xad, 0xbe, 0xef};
    unsigned passed = 0;
    uint32_t seq = 1;
    int fd = connect_reliability_client();
    int ret;

    if (fd < 0) {
        printf("AICP_RTTHREAD_RELIABILITY_SUMMARY passed=0 failed=%u\n",
               expected_cases);
        return fd;
    }
    ret = send_hello(fd, &seq);
    if (ret != 0) {
        goto done;
    }

    struct aicp_header heartbeat =
        make_header(AICP_MSG_HEARTBEAT, AICP_FLAG_ACK_REQUIRED, 0, seq++);
    ret = send_frame(fd, heartbeat, NULL);
    if (ret == 0) {
        ret = expect_reply(fd, AICP_MSG_STATUS, heartbeat.seq, AICP_OK,
                           &heartbeat_status);
    }
    if (report_reliability_case("heartbeat_status", ret, &passed) != 0) {
        goto done;
    }

    struct aicp_header control_header = make_header(
        AICP_MSG_CONTROL_SET, AICP_FLAG_ACK_REQUIRED, sizeof(control), seq++);
    ret = send_frame(fd, control_header, &control);
    if (ret == 0) {
        ret = expect_reply(fd, AICP_MSG_STATUS, control_header.seq, AICP_OK,
                           &control_status);
    }
    if (report_reliability_case("control_status", ret, &passed) != 0) {
        goto done;
    }

    control_header.flags |= AICP_FLAG_RETRANSMIT;
    ret = send_frame(fd, control_header, &control);
    if (ret == 0) {
        ret = expect_reply(fd, AICP_MSG_STATUS, control_header.seq, AICP_OK,
                           &duplicate_status);
    }
    if (ret == 0 && memcmp(&control_status, &duplicate_status,
                           sizeof(control_status)) != 0) {
        ret = -EPROTO;
    }
    if (report_reliability_case("duplicate_replay", ret, &passed) != 0) {
        goto done;
    }

    struct aicp_header stale =
        make_header(AICP_MSG_HEARTBEAT, AICP_FLAG_ACK_REQUIRED, 0, 2);
    ret = send_frame(fd, stale, NULL);
    if (ret == 0) {
        ret = expect_reply(fd, AICP_MSG_ERROR, stale.seq,
                           AICP_ERR_SEQUENCE, NULL);
    }
    if (report_reliability_case("stale_sequence", ret, &passed) != 0) {
        goto done;
    }

    struct aicp_header bad_version =
        make_header(AICP_MSG_HEARTBEAT, AICP_FLAG_ACK_REQUIRED, 0, seq);
    bad_version.version = AICP_VERSION + 1u;
    ret = send_raw_frame(fd, bad_version, NULL);
    if (ret == 0) {
        ret = expect_reply(fd, AICP_MSG_ERROR, bad_version.seq,
                           AICP_ERR_VERSION, NULL);
    }
    if (report_reliability_case("bad_version", ret, &passed) != 0) {
        goto done;
    }

    struct aicp_header bad_type = make_header(0xfeu, 0, 0, seq++);
    ret = send_frame(fd, bad_type, NULL);
    if (ret == 0) {
        ret = expect_reply(fd, AICP_MSG_ERROR, bad_type.seq,
                           AICP_ERR_BAD_TYPE, NULL);
    }
    if (report_reliability_case("bad_type", ret, &passed) != 0) {
        goto done;
    }

    struct aicp_header bad_length = make_header(
        AICP_MSG_CONTROL_SET, AICP_FLAG_ACK_REQUIRED,
        sizeof(bad_payload), seq++);
    ret = send_frame(fd, bad_length, bad_payload);
    if (ret == 0) {
        ret = expect_reply(fd, AICP_MSG_ERROR, bad_length.seq,
                           AICP_ERR_BAD_PAYLOAD, NULL);
    }
    if (report_reliability_case("bad_payload", ret, &passed) != 0) {
        goto done;
    }

    close(fd);
    fd = connect_reliability_client();
    if (fd < 0) {
        ret = fd;
        goto done;
    }
    seq = 1;
    ret = send_hello(fd, &seq);
    heartbeat =
        make_header(AICP_MSG_HEARTBEAT, AICP_FLAG_ACK_REQUIRED, 0, seq++);
    if (ret == 0) {
        ret = send_frame(fd, heartbeat, NULL);
    }
    if (ret == 0) {
        ret = expect_reply(fd, AICP_MSG_STATUS, heartbeat.seq, AICP_OK,
                           &heartbeat_status);
    }
    if (report_reliability_case("disconnect_reconnect", ret, &passed) != 0) {
        goto done;
    }

done:
    if (fd >= 0) {
        close(fd);
    }
    printf("AICP_RTTHREAD_RELIABILITY_SUMMARY passed=%u failed=%u\n",
           passed,
           expected_cases - passed);
    return passed == expected_cases ? 0 : -EPROTO;
}
#endif

struct udp_peer {
    int fd;
    struct sockaddr_in addr;
};

static int open_udp_peer(
    const char *host,
    uint16_t port,
    unsigned timeout_ms,
    struct udp_peer *peer,
    const char **stage) {
    if (stage != NULL) {
        *stage = "socket";
    }
    int fd = socket(AF_INET, SOCK_DGRAM, 0);
    if (fd < 0) {
        return -errno;
    }

    if (stage != NULL) {
        *stage = "setsockopt";
    }
    int ret = set_io_timeout(fd, timeout_ms, stage);
    if (ret != 0) {
        close(fd);
        return ret;
    }

    memset(peer, 0, sizeof(*peer));
    peer->fd = fd;
    peer->addr.sin_family = AF_INET;
    peer->addr.sin_port = htons(port);
    if (inet_pton(AF_INET, host, &peer->addr.sin_addr) != 1) {
        close(fd);
        return -EINVAL;
    }

    struct sockaddr_in local;
    memset(&local, 0, sizeof(local));
    local.sin_family = AF_INET;
    local.sin_port = htons(0);
    if (inet_pton(AF_INET, AICP_INIT_CLIENT, &local.sin_addr) != 1) {
        close(fd);
        return -EINVAL;
    }
    if (bind(fd, (const struct sockaddr *)&local, sizeof(local)) != 0) {
        int saved = errno;
        printf("AICP %s udp bind failed errno=%d\n", AICP_INIT_GUEST_LABEL, saved);
        close(fd);
        return -saved;
    }
    if (connect(fd, (const struct sockaddr *)&peer->addr, sizeof(peer->addr)) != 0) {
        int saved = errno;
        printf("AICP %s udp connect failed errno=%d\n", AICP_INIT_GUEST_LABEL, saved);
        close(fd);
        return -saved;
    }

    if (stage != NULL) {
        *stage = "ready";
    }
    return 0;
}

static int udp_send_frame(
    const struct udp_peer *peer,
    struct aicp_header hdr,
    const void *payload) {
    uint8_t wire[AICP_HEADER_LEN + AICP_MAX_PAYLOAD];
    size_t len = 0;
    int ret = aicp_datagram_encode(
        hdr, payload, wire, sizeof(wire), &len);
    if (ret != 0) {
        return ret;
    }
    printf("AICP %s udp send begin seq=%u type=%u len=%zu\n",
           AICP_INIT_GUEST_LABEL,
           hdr.seq,
           hdr.msg_type,
           len);
    ssize_t sent;
#if AICP_INIT_RAW_SYSCALL_IO
    printf("AICP %s udp send syscall=sendto nr=%ld fd=%d target=%s:%u\n",
           AICP_INIT_GUEST_LABEL,
           (long)SYS_sendto,
           peer->fd,
           AICP_INIT_SERVER,
           AICP_INIT_SERVER_PORT);
    sent = syscall(SYS_sendto,
                   peer->fd,
                   wire,
                   len,
                   0,
                   (const struct sockaddr *)&peer->addr,
                   sizeof(peer->addr));
#else
    sent = send(peer->fd, wire, len, 0);
#endif
    int saved = errno;
    printf("AICP %s udp send ret=%zd errno=%d\n", AICP_INIT_GUEST_LABEL, sent, sent < 0 ? saved : 0);
    if (sent < 0) {
        return -saved;
    }
    return (size_t)sent == len ? 0 : -EIO;
}

static int udp_recv_frame(
    const struct udp_peer *peer,
    struct aicp_header *hdr,
    void *payload,
    size_t capacity) {
    uint8_t wire[AICP_HEADER_LEN + AICP_MAX_PAYLOAD];
    struct sockaddr_in from;
    socklen_t from_len = sizeof(from);

    printf("AICP %s udp recv begin\n", AICP_INIT_GUEST_LABEL);
    ssize_t got;
#if AICP_INIT_RAW_SYSCALL_IO
    printf("AICP %s udp recv syscall=recvfrom nr=%ld fd=%d\n",
           AICP_INIT_GUEST_LABEL,
           (long)SYS_recvfrom,
           peer->fd);
    got = syscall(SYS_recvfrom, peer->fd, wire, sizeof(wire), 0, (struct sockaddr *)&from, &from_len);
#else
    got = recvfrom(peer->fd, wire, sizeof(wire), 0, (struct sockaddr *)&from, &from_len);
#endif
    int saved = errno;
    printf("AICP %s udp recv ret=%zd errno=%d\n", AICP_INIT_GUEST_LABEL, got, got < 0 ? saved : 0);
    if (got < 0) {
        return -saved;
    }
    return aicp_datagram_decode(
        wire, (size_t)got, hdr, payload, capacity);
}

static int udp_exchange(
    const struct udp_peer *peer,
    struct aicp_header tx,
    const void *tx_payload,
    struct aicp_header *rx,
    void *rx_payload,
    size_t rx_capacity,
    uint8_t expect_type,
    uint32_t expect_seq,
    uint64_t *rtt_ns) {
    int last_ret = -ETIMEDOUT;

    for (unsigned attempt = 0; attempt < AICP_INIT_UDP_RETRIES; attempt++) {
        struct aicp_header tx_attempt = tx;
        uint64_t start = monotonic_ns();
        if (attempt != 0) {
            tx_attempt.flags |= AICP_FLAG_RETRANSMIT;
        }
        int ret = udp_send_frame(peer, tx_attempt, tx_payload);
        if (ret != 0) {
            last_ret = ret;
            continue;
        }

        for (;;) {
            ret = udp_recv_frame(peer, rx, rx_payload, rx_capacity);
            if (ret != 0) {
                last_ret = ret;
                break;
            }
            if (rx->seq != expect_seq) {
                printf("AICP %s udp out_of_order got=%u expect=%u\n",
                       AICP_INIT_GUEST_LABEL,
                       rx->seq,
                       expect_seq);
                continue;
            }
            if (rx->msg_type == AICP_MSG_ERROR) {
                return rx->error_code == 0 ? -EPROTO : -(int)rx->error_code;
            }
            if (rx->msg_type != expect_type) {
                return -EPROTO;
            }
            if (rtt_ns != NULL) {
                *rtt_ns = monotonic_ns() - start;
            }
            return 0;
        }
    }
    return last_ret;
}

static int udp_send_hello(const struct udp_peer *peer, uint32_t *seq) {
    char payload[96];
    uint8_t rx_payload[AICP_MAX_PAYLOAD];
    struct aicp_header rx;
    uint32_t hello_seq = (*seq)++;

    snprintf(payload, sizeof(payload), "{\"role\":\"%s\",\"cap\":\"ai,control,status,udp\"}", AICP_INIT_ROLE);
    return udp_exchange(
        peer,
        make_header(AICP_MSG_HELLO, AICP_FLAG_ACK_REQUIRED, (uint32_t)strlen(payload) + 1u, hello_seq),
        payload,
        &rx,
        rx_payload,
        sizeof(rx_payload),
        AICP_MSG_STATUS,
        hello_seq,
        NULL);
}

static int udp_transact_control(
    const struct udp_peer *peer,
    uint32_t *seq,
    const struct aicp_control_payload *control,
    struct aicp_status_payload *status,
    uint64_t *rtt_ns) {
    uint8_t rx_payload[AICP_MAX_PAYLOAD];
    struct aicp_header rx;
    uint32_t control_seq = (*seq)++;

    int ret = udp_exchange(
        peer,
        make_header(AICP_MSG_CONTROL_SET, AICP_FLAG_ACK_REQUIRED, sizeof(*control), control_seq),
        control,
        &rx,
        rx_payload,
        sizeof(rx_payload),
        AICP_MSG_STATUS,
        control_seq,
        rtt_ns);
    if (ret != 0) {
        return ret;
    }
    if (rx.payload_len != sizeof(*status)) {
        return -EPROTO;
    }
    memcpy(status, rx_payload, sizeof(*status));
    return 0;
}

static int udp_test_stale_sequence(
    const struct udp_peer *peer,
    uint32_t stale_seq,
    const struct aicp_control_payload *control) {
    uint8_t rx_payload[AICP_MAX_PAYLOAD];
    struct aicp_header rx;
    int ret = udp_exchange(
        peer,
        make_header(AICP_MSG_CONTROL_SET, AICP_FLAG_ACK_REQUIRED, sizeof(*control), stale_seq),
        control,
        &rx,
        rx_payload,
        sizeof(rx_payload),
        AICP_MSG_STATUS,
        stale_seq,
        NULL);
    if (ret == -(int)AICP_ERR_SEQUENCE) {
        printf("AICP %s udp stale_sequence accepted=0 seq=%u error=%u\n",
               AICP_INIT_GUEST_LABEL,
               stale_seq,
               AICP_ERR_SEQUENCE);
        return 0;
    }
    printf("AICP %s udp stale_sequence unexpected_result seq=%u ret=%d\n",
           AICP_INIT_GUEST_LABEL,
           stale_seq,
           ret);
    return -EPROTO;
}

int main(void) {
    struct cpu_stat_sample cpu_before[AICP_CPU_STAT_MAX];
    struct cpu_stat_sample cpu_after[AICP_CPU_STAT_MAX];
    signal(SIGPIPE, SIG_IGN);
    setvbuf(stdout, NULL, _IONBF, 0);
    setvbuf(stderr, NULL, _IONBF, 0);
    ensure_virtual_fs();

    printf("AICP %s client starting server=%s:%u client=%s mode=%s transport=%s iterations=%u stress_procs=%u\n",
           AICP_INIT_GUEST_LABEL,
           AICP_INIT_SERVER,
           AICP_INIT_SERVER_PORT,
           AICP_INIT_CLIENT,
           AICP_INIT_MODE,
           AICP_INIT_TRANSPORT,
           AICP_INIT_ITERATIONS,
           AICP_INIT_STRESS_PROCS);

    int ret = 0;
    for (unsigned retry = 0; retry < 50; retry++) {
        ret = configure_iface();
        if (ret == 0) {
            break;
        }
        sleep_ms(100);
    }
    if (ret != 0) {
        fprintf(stderr, "AICP %s network config failed: %d\n", AICP_INIT_GUEST_LABEL, ret);
        return 1;
    }
    printf("AICP %s %s configured ip=%s netmask=%s\n",
           AICP_INIT_GUEST_LABEL,
           AICP_INIT_IFACE,
           AICP_INIT_CLIENT,
           AICP_INIT_NETMASK);
    dump_iface_diag("configured");
#if AICP_INIT_RELIABILITY_TEST
    if (strcmp(AICP_INIT_TRANSPORT, "tcp") != 0) {
        fprintf(stderr, "AICP_RTTHREAD_RELIABILITY requires TCP transport\n");
        return 1;
    }
    ret = run_rtthread_reliability_tests();
    if (ret != 0) {
        printf("%s ok=0 failed=1 avg_rtt_ns=0 max_rtt_ns=0\n",
               AICP_INIT_DONE_TOKEN);
        sync();
        reboot(RB_POWER_OFF);
        return 1;
    }
#endif
    start_stress_procs();
    read_cpu_stats(cpu_before);
    uint64_t test_start_ns = monotonic_ns();

    int fd = -1;
    struct udp_peer udp;
    memset(&udp, 0, sizeof(udp));
    udp.fd = -1;
    uint32_t seq = 1;
    unsigned ok = 0;
    unsigned failed = 0;
    uint64_t total_rtt = 0;
    uint64_t max_rtt = 0;
    const int ai_mode = strcmp(AICP_INIT_MODE, "fixed") != 0;
    const int udp_mode = strcmp(AICP_INIT_TRANSPORT, "udp") == 0;

    if (udp_mode) {
        const char *udp_stage = "start";
        ret = open_udp_peer(AICP_INIT_SERVER, (uint16_t)AICP_INIT_SERVER_PORT, 1000, &udp, &udp_stage);
        if (ret != 0) {
            printf("AICP %s udp open failed stage=%s ret=%d\n", AICP_INIT_GUEST_LABEL, udp_stage, ret);
            failed = AICP_INIT_ITERATIONS;
        } else {
            printf("AICP %s udp ready target=%s:%u retries=%u\n",
                   AICP_INIT_GUEST_LABEL,
                   AICP_INIT_SERVER,
                   AICP_INIT_SERVER_PORT,
                   AICP_INIT_UDP_RETRIES);
            ret = udp_send_hello(&udp, &seq);
            if (ret != 0) {
                printf("AICP %s UDP HELLO failed ret=%d\n", AICP_INIT_GUEST_LABEL, ret);
                failed = AICP_INIT_ITERATIONS;
            } else {
                printf("AICP %s connected transport=udp\n", AICP_INIT_GUEST_LABEL);
            }
        }
    }

    for (unsigned i = 0; ok < AICP_INIT_ITERATIONS && failed < AICP_INIT_ITERATIONS; i++) {
        if (!udp_mode && fd < 0) {
            unsigned connect_retry = 0;
            while (connect_retry < AICP_INIT_CONNECT_RETRIES) {
                const char *connect_stage = "start";
                fd = connect_tcp(AICP_INIT_SERVER,
                                 (uint16_t)AICP_INIT_SERVER_PORT,
                                 AICP_INIT_TCP_TIMEOUT_MS,
                                 &connect_stage);
                if (fd >= 0) {
                    break;
                }
                printf("AICP %s connect retry=%u stage=%s ret=%d\n", AICP_INIT_GUEST_LABEL, connect_retry + 1, connect_stage, fd);
                if ((connect_retry % 5u) == 0) {
                    dump_iface_diag("connect-failed");
                }
                connect_retry++;
                sleep_ms(200);
            }
            if (fd < 0) {
                failed = AICP_INIT_ITERATIONS - ok;
                printf("AICP %s connect giveup retries=%u\n", AICP_INIT_GUEST_LABEL, AICP_INIT_CONNECT_RETRIES);
                break;
            }
            ret = send_hello(fd, &seq);
            if (ret != 0) {
                close(fd);
                fd = -1;
                failed = AICP_INIT_ITERATIONS - ok;
                printf("AICP %s HELLO failed ret=%d\n", AICP_INIT_GUEST_LABEL, ret);
                sleep_ms(200);
                break;
            }
            printf("AICP %s connected\n", AICP_INIT_GUEST_LABEL);
        }

        const struct aicp_control_payload control =
            aicp_control_policy(i, ai_mode != 0);
        struct aicp_status_payload status;
        uint64_t rtt_ns = 0;
        ret = udp_mode
                  ? udp_transact_control(&udp, &seq, &control, &status, &rtt_ns)
                  : transact_control(fd, &seq, &control, &status, &rtt_ns);
        if (ret != 0) {
            if (!udp_mode) {
                close(fd);
                fd = -1;
            }
            failed++;
            printf("AICP %s transaction failed ret=%d\n", AICP_INIT_GUEST_LABEL, ret);
            dump_iface_diag("transaction-failed");
            sleep_ms(200);
            continue;
        }

        ok++;
        total_rtt += rtt_ns;
        if (rtt_ns > max_rtt) {
            max_rtt = rtt_ns;
        }
        printf("%s seq=%u target=%.3f measured=%.3f error=%.3f rtt_ns=%llu\n",
               AICP_INIT_STATUS_TOKEN,
               status.applied_seq,
               control.target,
               status.measured,
               status.error,
               (unsigned long long)rtt_ns);
        if (udp_mode && AICP_INIT_UDP_REORDER_TEST && i == 0) {
            ret = udp_test_stale_sequence(&udp, seq - 2u, &control);
            if (ret != 0) {
                failed++;
                printf("AICP %s udp stale_sequence test failed ret=%d\n",
                       AICP_INIT_GUEST_LABEL,
                       ret);
            }
        }
        if (ok < AICP_INIT_ITERATIONS && failed < AICP_INIT_ITERATIONS) {
            sleep_ms(20);
        }
    }

    if (fd >= 0) {
        close(fd);
    }
    if (udp.fd >= 0) {
        close(udp.fd);
    }

    uint64_t avg_rtt = ok == 0 ? 0 : total_rtt / ok;
    uint64_t test_duration_ns = monotonic_ns() - test_start_ns;
    read_cpu_stats(cpu_after);
    print_cpu_usage(cpu_before, cpu_after);
    printf("AICP_LINUX_RUNTIME duration_ns=%llu iterations=%u stress_procs=%u\n",
           (unsigned long long)test_duration_ns, ok + failed, AICP_INIT_STRESS_PROCS);
    dump_iface_diag("completed");
    printf("%s ok=%u failed=%u avg_rtt_ns=%llu max_rtt_ns=%llu\n",
           AICP_INIT_DONE_TOKEN,
           ok,
           failed,
           (unsigned long long)avg_rtt,
           (unsigned long long)max_rtt);
    sync();
    reboot(RB_POWER_OFF);
    return failed == 0 && ok > 0 ? 0 : 1;
}
