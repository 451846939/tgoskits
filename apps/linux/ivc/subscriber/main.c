#include <ivc/ulib.h>
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>

#define AXIVC_PERF_SHM_BASE 0xbff00000UL
#define AXIVC_PERF_SHM_SIZE 0x1000000UL
#define AXIVC_PERF_MAGIC 0x49565046U
#define AXIVC_PERF_VERSION 1U
#define AXIVC_PERF_ITERATIONS 100U
#define AXIVC_PERF_TEST_COUNT 4U
#define AXIVC_PERF_STATE_IDLE 0U
#define AXIVC_PERF_STATE_READY 1U
#define AXIVC_PERF_STATE_DONE 2U
#define AXIVC_PERF_STATE_COMPLETE 3U

struct axivc_region_header {
    uint32_t magic;
    uint32_t version;
    uint32_t header_size;
    uint32_t region_size;
    uint32_t features;
    uint32_t publisher_to_subscriber_offset;
    uint32_t subscriber_to_publisher_offset;
    uint32_t ring_size;
} __attribute__((aligned(8)));

struct axivc_message_slot {
    uint64_t sequence;
    uint32_t len;
    uint32_t kind;
    uint8_t payload[48];
} __attribute__((aligned(64)));

struct axivc_ring {
    uint32_t direction;
    uint32_t capacity;
    uint32_t slot_payload_size;
    uint32_t head;
    uint32_t tail;
    uint32_t reserved[3];
    struct axivc_message_slot slots[16];
} __attribute__((aligned(64)));

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
} __attribute__((aligned(64)));

struct axivc_region {
    uint64_t publisher_id;
    uint64_t key;
    struct axivc_region_header header;
    struct axivc_ring publisher_to_subscriber;
    struct axivc_ring subscriber_to_publisher;
    struct axivc_perf_control perf;
} __attribute__((aligned(64)));

static const uint32_t perf_sizes[AXIVC_PERF_TEST_COUNT] = {
    256U * 1024U,
    512U * 1024U,
    1024U * 1024U,
    10U * 1024U * 1024U,
};

static const char *perf_size_labels[AXIVC_PERF_TEST_COUNT] = {
    "256KB",
    "512KB",
    "1MB",
    "10MB",
};

char message[1024];

static uint64_t now_ns(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ULL + (uint64_t)ts.tv_nsec;
}

static uint64_t checksum_payload(volatile uint8_t *payload, size_t bytes)
{
    uint64_t checksum = 0;
    for (size_t i = 0; i < bytes; i++) {
        checksum += payload[i];
    }
    return checksum;
}

static uint32_t load_state(struct axivc_perf_control *perf)
{
    return __atomic_load_n(&perf->state, __ATOMIC_ACQUIRE);
}

static void store_state(struct axivc_perf_control *perf, uint32_t state)
{
    __atomic_store_n(&perf->state, state, __ATOMIC_RELEASE);
}

static double throughput_gbps(uint64_t bytes, uint64_t iterations, uint64_t ns)
{
    if (ns == 0) {
        return 0.0;
    }
    return (double)bytes * (double)iterations * 8.0 / (double)ns;
}

static void print_perf_summary(const uint64_t read_ns[AXIVC_PERF_TEST_COUNT],
                               const uint64_t write_ns[AXIVC_PERF_TEST_COUNT])
{
    printf("ivc perf summary iterations=%u unit=Gbps\n", AXIVC_PERF_ITERATIONS);
    for (uint32_t test = 0; test < AXIVC_PERF_TEST_COUNT; test++) {
        printf("ivc perf summary size=%s read=%.3f write=%.3f\n",
               perf_size_labels[test],
               throughput_gbps(perf_sizes[test], AXIVC_PERF_ITERATIONS, read_ns[test]),
               throughput_gbps(perf_sizes[test], AXIVC_PERF_ITERATIONS, write_ns[test]));
    }
}

static int wait_for_perf_magic(struct axivc_perf_control *perf)
{
    for (unsigned long polls = 0; polls < 200000; polls++) {
        uint32_t magic = __atomic_load_n(&perf->magic, __ATOMIC_ACQUIRE);
        uint32_t version = __atomic_load_n(&perf->version, __ATOMIC_ACQUIRE);
        if (magic == AXIVC_PERF_MAGIC && version == AXIVC_PERF_VERSION) {
            return 0;
        }
        usleep(1000);
    }
    fprintf(stderr, "Timed out waiting for Zephyr IVC perf region\n");
    return -1;
}

static int run_perf_subscriber(uint64_t target_publisher_id, uint64_t channel_key)
{
    int ret = 0;
    int memfd = -1;
    void *mapping = MAP_FAILED;
    ivc_manager_p manager = NULL;
    ivc_subscriber_p subscriber = NULL;
    struct axivc_region *region;
    struct axivc_perf_control *perf;
    volatile uint8_t *payload;
    uint64_t read_ns[AXIVC_PERF_TEST_COUNT] = {0};
    uint64_t write_ns[AXIVC_PERF_TEST_COUNT] = {0};

    manager = ivc_open_manager();
    if (!manager) {
        fprintf(stderr, "Failed to open IVC manager\n");
        return 1;
    }

    subscriber = ivc_subscribe(manager, target_publisher_id, channel_key);
    if (!subscriber) {
        fprintf(stderr, "Failed to subscribe to channel\n");
        ret = 2;
        goto out_close_manager;
    }

    memfd = open("/dev/mem", O_RDWR | O_SYNC);
    if (memfd < 0) {
        perror("Failed to open /dev/mem");
        ret = 3;
        goto out_unsubscribe;
    }

    mapping = mmap(NULL, AXIVC_PERF_SHM_SIZE, PROT_READ | PROT_WRITE,
                   MAP_SHARED, memfd, AXIVC_PERF_SHM_BASE);
    if (mapping == MAP_FAILED) {
        perror("Failed to mmap IVC shared memory");
        ret = 4;
        goto out_close_mem;
    }

    region = (struct axivc_region *)mapping;
    perf = &region->perf;
    payload = (volatile uint8_t *)mapping + ((sizeof(struct axivc_region) + 63U) & ~63U);

    if (wait_for_perf_magic(perf) < 0) {
        ret = 5;
        goto out_unmap;
    }

    while (1) {
        uint32_t state = load_state(perf);
        if (state == AXIVC_PERF_STATE_COMPLETE) {
            break;
        }
        if (state != AXIVC_PERF_STATE_READY) {
            usleep(50);
            continue;
        }

        uint32_t test = __atomic_load_n(&perf->test_index, __ATOMIC_ACQUIRE);
        uint32_t iter = __atomic_load_n(&perf->iteration, __ATOMIC_ACQUIRE);
        uint64_t bytes = __atomic_load_n(&perf->bytes, __ATOMIC_ACQUIRE);
        uint64_t expected = __atomic_load_n(&perf->checksum, __ATOMIC_ACQUIRE);
        uint64_t write_iter_ns = __atomic_load_n(&perf->write_ns, __ATOMIC_ACQUIRE);
        uint64_t start = now_ns();
        uint64_t actual = checksum_payload(payload, bytes);
        uint64_t end = now_ns();

        if (test >= AXIVC_PERF_TEST_COUNT || bytes != perf_sizes[test] ||
            iter >= AXIVC_PERF_ITERATIONS) {
            fprintf(stderr, "Invalid IVC perf descriptor test=%u iter=%u bytes=%" PRIu64 "\n",
                    test, iter, bytes);
            ret = 6;
            break;
        }
        if (actual != expected) {
            fprintf(stderr,
                    "IVC perf checksum mismatch test=%u iter=%u bytes=%" PRIu64
                    " expected=%" PRIu64 " actual=%" PRIu64 "\n",
                    test, iter, bytes, expected, actual);
            ret = 7;
            break;
        }

        read_ns[test] += end - start;
        write_ns[test] += write_iter_ns;
        if (iter + 1 == AXIVC_PERF_ITERATIONS) {
            printf("linux ivc read size=%" PRIu64 " iterations=%u avg=%.3f Gbps\n",
                   bytes, AXIVC_PERF_ITERATIONS,
                   throughput_gbps(bytes, AXIVC_PERF_ITERATIONS, read_ns[test]));
        }
        store_state(perf, AXIVC_PERF_STATE_DONE);
    }

    if (ret == 0) {
        print_perf_summary(read_ns, write_ns);
        printf("ivc perf test pass\n");
    } else {
        printf("ivc perf test failed\n");
    }

out_unmap:
    if (mapping != MAP_FAILED) {
        munmap(mapping, AXIVC_PERF_SHM_SIZE);
    }
out_close_mem:
    if (memfd >= 0) {
        close(memfd);
    }
out_unsubscribe:
    if (subscriber && ivc_unsubscribe(subscriber) < 0 && ret == 0) {
        fprintf(stderr, "Failed to unsubscribe from channel\n");
        ret = 8;
    }
out_close_manager:
    if (manager && ivc_close_manager(manager) < 0 && ret == 0) {
        fprintf(stderr, "Failed to close IVC manager\n");
        ret = 9;
    }
    printf("IVC perf subscriber example finished.\n");
    return ret;
}

int main(int argc, char *argv[]) {
    unsigned long target_count = 5;
    unsigned long received = 0;
    unsigned long empty_polls = 0;

    if (argc != 3 && argc != 4) {
        fprintf(stderr, "Usage: %s <target_publisher_id> <channel_key> [message_count]\n", argv[0]);
        return 1;
    }
    uint64_t target_publisher_id = strtoull(argv[1], NULL, 0);
    uint64_t channel_key = strtoull(argv[2], NULL, 0);
    if (argc == 4) {
        if (strcmp(argv[3], "perf") == 0) {
            return run_perf_subscriber(target_publisher_id, channel_key);
        }
        target_count = strtoul(argv[3], NULL, 0);
    }

    int ret = 0;

    ivc_manager_p manager = ivc_open_manager();
    if (!manager) {
        fprintf(stderr, "Failed to open IVC manager\n");
        return 1;
    }

    ivc_subscriber_p subscriber = ivc_subscribe(manager, target_publisher_id, channel_key);
    if (!subscriber) {
        fprintf(stderr, "Failed to subscribe to channel\n");
        ret = 2;
        goto close_manager;
    }

    while (received < target_count) {
        int bytes_read = ivc_read(subscriber, message, sizeof(message) - 1);
        if (bytes_read < 0) {
            fprintf(stderr, "Failed to read from subscriber\n");
            ret = 2;
            break;
        } else if (bytes_read == 0) {
            if (++empty_polls > 200000) {
                fprintf(stderr, "Timed out waiting for IVC messages\n");
                ret = 5;
                break;
            }
            usleep(10000);
        } else {
            message[bytes_read] = '\0'; // Null-terminate the string
            received++;
            empty_polls = 0;
            printf("linux ivc recv %lu/%lu: %s\n", received, target_count, message);
        }
    }
    if (ret == 0) {
        printf("linux ivc demo pass\n");
    }

    if (ivc_unsubscribe(subscriber) < 0) {
        fprintf(stderr, "Failed to unsubscribe from channel\n");
        ret = 3;
    }
close_manager:
    if (ivc_close_manager(manager) < 0) {
        fprintf(stderr, "Failed to close IVC manager\n");
        ret = 4;
    }
    printf("IVC subscriber example finished.\n");
    return ret;
}
