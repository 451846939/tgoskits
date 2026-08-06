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

#define AXIVC_PERF_LEGACY_SHM_BASE 0xbff00000UL
#define AXIVC_PERF_LEGACY_SHM_SIZE 0x2000000UL
#define AXIVC_PERF_MAGIC 0x49565046U
#define AXIVC_PERF_VERSION 1U
#define AXIVC_PERF_ITERATIONS 100U
#define AXIVC_PERF_TEST_COUNT 4U
#define AXIVC_PERF_STATE_IDLE 0U
#define AXIVC_PERF_STATE_READY 1U
#define AXIVC_PERF_STATE_DONE 2U
#define AXIVC_PERF_STATE_COMPLETE 3U
#define AXIVC_PERF_PAYLOAD_MAX (10U * 1024U * 1024U)
#define AXIVC_PERF_DATA_OFFSET 0x10000U
#define AXIVC_PERF_READ_MEM_OFFSET AXIVC_PERF_DATA_OFFSET
#define AXIVC_PERF_WRITE_MEM_OFFSET (AXIVC_PERF_READ_MEM_OFFSET + AXIVC_PERF_PAYLOAD_MAX)
#define AXIVC_SHM_CACHE_POLICY "normal-cacheable/coherent-required"

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
    uint64_t zephyr_copy_ns;
    uint64_t reserved1;
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

static uint8_t prng_next(uint32_t *state)
{
    *state = (*state * 1664525U) + 1013904223U;
    return (uint8_t)(*state >> 24);
}

static void fill_random(uint8_t *buf, size_t len, uint32_t seed)
{
    uint32_t state = seed;

    for (size_t i = 0; i < len; i++) {
        buf[i] = prng_next(&state);
    }
}

static uint64_t now_ns(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ULL + (uint64_t)ts.tv_nsec;
}

static uint64_t parse_u64_env(const char *name, uint64_t fallback)
{
    const char *value = getenv(name);
    char *end = NULL;
    uint64_t parsed;

    if (!value || value[0] == '\0') {
        return fallback;
    }

    errno = 0;
    parsed = strtoull(value, &end, 0);
    if (errno != 0 || end == value || *end != '\0') {
        fprintf(stderr, "Invalid %s value '%s', using 0x%" PRIx64 "\n",
                name, value, fallback);
        return fallback;
    }
    return parsed;
}

static uint32_t load_state(struct axivc_perf_control *perf)
{
    uint32_t state = __atomic_load_n(&perf->state, __ATOMIC_ACQUIRE);

    __atomic_thread_fence(__ATOMIC_ACQUIRE);
    return state;
}

static void store_state(struct axivc_perf_control *perf, uint32_t state)
{
    __atomic_thread_fence(__ATOMIC_RELEASE);
    __atomic_store_n(&perf->state, state, __ATOMIC_RELEASE);
}

static void publish_perf_request(struct axivc_perf_control *perf,
                                 uint32_t test, uint32_t iter, uint64_t bytes)
{
    __atomic_store_n(&perf->test_index, test, __ATOMIC_RELAXED);
    __atomic_store_n(&perf->iteration, iter, __ATOMIC_RELAXED);
    __atomic_store_n(&perf->bytes, bytes, __ATOMIC_RELAXED);
    store_state(perf, AXIVC_PERF_STATE_READY);
}

static uint64_t consume_zephyr_copy_ns(struct axivc_perf_control *perf)
{
    return __atomic_load_n(&perf->zephyr_copy_ns, __ATOMIC_ACQUIRE);
}

static double throughput_gbps(uint64_t bytes, uint64_t iterations, uint64_t ns)
{
    if (ns == 0) {
        return 0.0;
    }
    return (double)bytes * (double)iterations * 8.0 / (double)ns;
}

static void print_perf_summary(const uint64_t read_ns[AXIVC_PERF_TEST_COUNT],
                               const uint64_t write_ns[AXIVC_PERF_TEST_COUNT],
                               const uint64_t zephyr_copy_ns[AXIVC_PERF_TEST_COUNT])
{
    printf("ivc perf summary iterations=%u unit=Gbps\n", AXIVC_PERF_ITERATIONS);
    for (uint32_t test = 0; test < AXIVC_PERF_TEST_COUNT; test++) {
        printf("ivc perf summary size=%s read=%.3f write=%.3f\n",
               perf_size_labels[test],
               throughput_gbps(perf_sizes[test], AXIVC_PERF_ITERATIONS, read_ns[test]),
               throughput_gbps(perf_sizes[test], AXIVC_PERF_ITERATIONS, write_ns[test]));
    }
}

static void print_perf_breakdown(
    const uint64_t fill_ns[AXIVC_PERF_TEST_COUNT],
    const uint64_t local_copy_ns[AXIVC_PERF_TEST_COUNT],
    const uint64_t write_ns[AXIVC_PERF_TEST_COUNT],
    const uint64_t publish_ns[AXIVC_PERF_TEST_COUNT],
    const uint64_t wait_done_ns[AXIVC_PERF_TEST_COUNT],
    const uint64_t read_ns[AXIVC_PERF_TEST_COUNT],
    const uint64_t verify_ns[AXIVC_PERF_TEST_COUNT],
    const uint64_t idle_ns[AXIVC_PERF_TEST_COUNT])
{
    for (uint32_t test = 0; test < AXIVC_PERF_TEST_COUNT; test++) {
        uint64_t e2e_ns = write_ns[test] + publish_ns[test] + wait_done_ns[test] +
                          read_ns[test] + verify_ns[test] + idle_ns[test];
        double iter = (double)AXIVC_PERF_ITERATIONS;
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
    size_t mapping_size = 0;
    const char *mmap_mode = getenv("AXIVC_MMAP");
    int force_devmem = mmap_mode && strcmp(mmap_mode, "devmem") == 0;
    int force_driver = mmap_mode && strcmp(mmap_mode, "driver") == 0;
    ivc_manager_p manager = NULL;
    ivc_subscriber_p subscriber = NULL;
    struct axivc_region *region;
    struct axivc_perf_control *perf;
    uint8_t *read_mem;
    uint8_t *write_mem;
    uint8_t *source = NULL;
    uint8_t *sink = NULL;
    uint64_t read_ns[AXIVC_PERF_TEST_COUNT] = {0};
    uint64_t write_ns[AXIVC_PERF_TEST_COUNT] = {0};
    uint64_t zephyr_copy_ns[AXIVC_PERF_TEST_COUNT] = {0};
    uint64_t fill_ns[AXIVC_PERF_TEST_COUNT] = {0};
    uint64_t local_copy_ns[AXIVC_PERF_TEST_COUNT] = {0};
    uint64_t publish_ns[AXIVC_PERF_TEST_COUNT] = {0};
    uint64_t wait_done_ns[AXIVC_PERF_TEST_COUNT] = {0};
    uint64_t verify_ns[AXIVC_PERF_TEST_COUNT] = {0};
    uint64_t idle_ns[AXIVC_PERF_TEST_COUNT] = {0};
    uint64_t shm_base = 0;
    uint64_t shm_size = 0;

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

    shm_base = ivc_subscriber_shm_base(subscriber);
    shm_size = ivc_subscriber_shm_size(subscriber);
    if (shm_size == 0) {
        shm_size = AXIVC_PERF_LEGACY_SHM_SIZE;
    }
    mapping_size = (size_t)shm_size;

    if (!force_devmem) {
        mapping = ivc_mmap_subscriber(subscriber, mapping_size);
        if (mapping != MAP_FAILED) {
            printf("linux ivc mmap source=driver\n");
        } else {
            perror("Failed to mmap IVC shared memory from subscriber device");
            if (force_driver) {
                ret = 4;
                goto out_unsubscribe;
            }
        }
    }

    if (mapping == MAP_FAILED) {
        if (shm_base == 0) {
            shm_base = AXIVC_PERF_LEGACY_SHM_BASE;
        }
        shm_base = parse_u64_env("AXIVC_DEVMEM_BASE", shm_base);

        memfd = open("/dev/mem", O_RDWR | O_SYNC);
        if (memfd < 0) {
            perror("Failed to open /dev/mem");
            ret = 3;
            goto out_unsubscribe;
        }

        mapping = mmap(NULL, mapping_size, PROT_READ | PROT_WRITE,
                       MAP_SHARED, memfd, (off_t)shm_base);
        if (mapping == MAP_FAILED) {
            perror("Failed to mmap IVC shared memory from /dev/mem");
            ret = 4;
            goto out_close_mem;
        }
        printf("linux ivc mmap source=devmem base=0x%" PRIx64 "\n", shm_base);
    }
    printf("linux ivc shm cache=%s\n", AXIVC_SHM_CACHE_POLICY);
    printf("linux ivc shm size=0x%" PRIx64 " info=%s\n", shm_size,
           ivc_subscriber_has_channel_info(subscriber) ? "driver" : "legacy");

    region = (struct axivc_region *)mapping;
    perf = &region->perf;
    read_mem = (uint8_t *)mapping + AXIVC_PERF_READ_MEM_OFFSET;
    write_mem = (uint8_t *)mapping + AXIVC_PERF_WRITE_MEM_OFFSET;

    if (AXIVC_PERF_WRITE_MEM_OFFSET + AXIVC_PERF_PAYLOAD_MAX > mapping_size) {
        fprintf(stderr, "IVC perf shared memory layout exceeds channel size\n");
        ret = 5;
        goto out_unmap;
    }

    source = malloc(AXIVC_PERF_PAYLOAD_MAX);
    sink = malloc(AXIVC_PERF_PAYLOAD_MAX);
    if (!source || !sink) {
        fprintf(stderr, "Failed to allocate local IVC perf buffers\n");
        ret = 6;
        goto out_free_buffers;
    }

    if (wait_for_perf_magic(perf) < 0) {
        ret = 7;
        goto out_free_buffers;
    }

    for (uint32_t test = 0; test < AXIVC_PERF_TEST_COUNT && ret == 0; test++) {
        size_t bytes = perf_sizes[test];

        for (uint32_t iter = 0; iter < AXIVC_PERF_ITERATIONS; iter++) {
            uint64_t start;
            uint64_t end;

            start = now_ns();
            fill_random(source, bytes, 0x49564300U ^ (test << 16) ^ iter);
            end = now_ns();
            fill_ns[test] += end - start;

            start = now_ns();
            memcpy(sink, source, bytes);
            end = now_ns();
            local_copy_ns[test] += end - start;

            start = now_ns();
            memcpy(read_mem, source, bytes);
            end = now_ns();
            write_ns[test] += end - start;

            start = now_ns();
            publish_perf_request(perf, test, iter, bytes);
            end = now_ns();
            publish_ns[test] += end - start;

            start = now_ns();
            while (load_state(perf) != AXIVC_PERF_STATE_DONE) {
                usleep(50);
            }
            end = now_ns();
            wait_done_ns[test] += end - start;
            zephyr_copy_ns[test] += consume_zephyr_copy_ns(perf);

            start = now_ns();
            memcpy(sink, write_mem, bytes);
            end = now_ns();
            read_ns[test] += end - start;

            start = now_ns();
            if (memcmp(source, sink, bytes) != 0) {
                fprintf(stderr,
                        "IVC perf data mismatch test=%u iter=%u bytes=%zu\n",
                        test, iter, bytes);
                ret = 8;
                break;
            }
            end = now_ns();
            verify_ns[test] += end - start;

            start = now_ns();
            store_state(perf, AXIVC_PERF_STATE_IDLE);
            end = now_ns();
            idle_ns[test] += end - start;
        }

        if (ret == 0) {
            printf("linux ivc write size=%zu iterations=%u avg=%.3f Gbps\n",
                   bytes, AXIVC_PERF_ITERATIONS,
                   throughput_gbps(bytes, AXIVC_PERF_ITERATIONS, write_ns[test]));
            printf("linux ivc read size=%zu iterations=%u avg=%.3f Gbps\n",
                   bytes, AXIVC_PERF_ITERATIONS,
                   throughput_gbps(bytes, AXIVC_PERF_ITERATIONS, read_ns[test]));
        }
    }

    if (ret == 0) {
        while (load_state(perf) != AXIVC_PERF_STATE_COMPLETE) {
            if (load_state(perf) == AXIVC_PERF_STATE_IDLE) {
                usleep(50);
            } else {
                usleep(50);
            }
        }
        print_perf_summary(read_ns, write_ns, zephyr_copy_ns);
        print_perf_breakdown(fill_ns, local_copy_ns, write_ns, publish_ns,
                             wait_done_ns, read_ns, verify_ns, idle_ns);
        printf("ivc perf test pass\n");
    } else {
        printf("ivc perf test failed\n");
    }

out_free_buffers:
    free(sink);
    free(source);
out_unmap:
    if (mapping != MAP_FAILED) {
        munmap(mapping, mapping_size);
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
