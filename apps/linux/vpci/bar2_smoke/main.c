#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <time.h>
#include <unistd.h>

#define VPCI_DEVICE "/sys/bus/pci/devices/0000:00:05.0"
#define BAR_SIZE 0x200000UL
#define PAYLOAD_SIZE 4096
#define MAGIC 0x41584232U
#define A_TO_B_SEQ 1U
#define B_TO_A_SEQ 2U
#define DOORBELL_STATUS 1U
#define BAR0_INT_STATUS_WORD 3
#define BAR0_DOORBELL_WORD 4
#define TIMEOUT_NS (15ULL * 1000ULL * 1000ULL * 1000ULL)

struct bar2_mailbox {
    volatile uint32_t magic;
    volatile uint32_t a_seq;
    volatile uint32_t b_seq;
    volatile uint32_t a_checksum;
    volatile uint32_t b_checksum;
    volatile uint8_t a_payload[PAYLOAD_SIZE];
    volatile uint8_t b_payload[PAYLOAD_SIZE];
};

static void die(const char *msg)
{
    fprintf(stderr, "vpci bar2 smoke failed: %s: %s\n", msg, strerror(errno));
    sync();
    _exit(1);
}

static uint64_t monotonic_ns(void)
{
    struct timespec ts;
    if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0) {
        die("clock_gettime");
    }
    return (uint64_t)ts.tv_sec * 1000000000ULL + (uint64_t)ts.tv_nsec;
}

static void wait_for(volatile uint32_t *field, uint32_t value, const char *what)
{
    uint64_t start = monotonic_ns();
    while (*field != value) {
        if (monotonic_ns() - start > TIMEOUT_NS) {
            fprintf(stderr, "vpci bar2 smoke failed: timeout waiting for %s\n", what);
            sync();
            _exit(1);
        }
        usleep(1000);
    }
    __sync_synchronize();
}

static void wait_status(volatile uint32_t *bar0, const char *what)
{
    uint64_t start = monotonic_ns();
    while ((bar0[BAR0_INT_STATUS_WORD] & DOORBELL_STATUS) == 0) {
        if (monotonic_ns() - start > TIMEOUT_NS) {
            fprintf(stderr, "vpci bar2 smoke failed: timeout waiting for %s\n", what);
            sync();
            _exit(1);
        }
        usleep(1000);
    }
    __sync_synchronize();
    bar0[BAR0_INT_STATUS_WORD] = DOORBELL_STATUS;
    __sync_synchronize();
}

static void write_doorbell(volatile uint32_t *bar0, uint32_t target_peer, uint32_t vector)
{
    bar0[BAR0_DOORBELL_WORD] = (target_peer << 16) | (vector & 0xffffU);
    __sync_synchronize();
}

static void ensure_sysfs(void)
{
    mkdir("/sys", 0555);
    if (mount("sysfs", "/sys", "sysfs", 0, "") != 0 && errno != EBUSY) {
        die("mount sysfs");
    }
}

static void *map_resource(const char *name, size_t size)
{
    char path[128];
    snprintf(path, sizeof(path), "%s/%s", VPCI_DEVICE, name);
    int fd = open(path, O_RDWR | O_SYNC);
    if (fd < 0) {
        die(path);
    }
    void *addr = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    close(fd);
    if (addr == MAP_FAILED) {
        die(path);
    }
    return addr;
}

static uint32_t checksum(const volatile uint8_t *data, size_t len)
{
    uint32_t sum = 0x12345678U;
    for (size_t i = 0; i < len; i++) {
        sum = (sum << 5) | (sum >> 27);
        sum ^= data[i];
        sum += (uint32_t)i;
    }
    return sum;
}

static void fill_payload(volatile uint8_t *data, size_t len, uint32_t seed)
{
    uint32_t x = seed;
    for (size_t i = 0; i < len; i++) {
        x = x * 1664525U + 1013904223U;
        data[i] = (uint8_t)(x >> 24);
    }
}

static void clear_mailbox(struct bar2_mailbox *box)
{
    volatile uint8_t *bytes = (volatile uint8_t *)box;
    for (size_t i = 0; i < sizeof(*box); i++) {
        bytes[i] = 0;
    }
    __sync_synchronize();
}

static void peer0(volatile uint32_t *bar0, struct bar2_mailbox *box)
{
    clear_mailbox(box);
    fill_payload(box->a_payload, PAYLOAD_SIZE, 0xa11c0000U);
    box->a_checksum = checksum(box->a_payload, PAYLOAD_SIZE);
    __sync_synchronize();
    box->magic = MAGIC;
    box->a_seq = A_TO_B_SEQ;
    __sync_synchronize();
    puts("VM A writes BAR2");
    write_doorbell(bar0, 1, 1);
    puts("VM A writes doorbell(target=B)");

    wait_for(&box->b_seq, B_TO_A_SEQ, "VM B response");
    if (checksum(box->b_payload, PAYLOAD_SIZE) != box->b_checksum) {
        fprintf(stderr, "vpci bar2 smoke failed: VM A checksum mismatch\n");
        _exit(1);
    }
    puts("VM A reads same data");
    wait_status(bar0, "VM B doorbell");
    puts("VM A observes doorbell event");
    puts("vpci bar2 shared memory pass");
}

static void peer1(volatile uint32_t *bar0, struct bar2_mailbox *box)
{
    wait_for(&box->magic, MAGIC, "BAR2 magic");
    wait_for(&box->a_seq, A_TO_B_SEQ, "VM A payload");
    wait_status(bar0, "VM A doorbell");
    puts("VM B observes doorbell event");
    if (checksum(box->a_payload, PAYLOAD_SIZE) != box->a_checksum) {
        fprintf(stderr, "vpci bar2 smoke failed: VM B checksum mismatch\n");
        _exit(1);
    }
    puts("VM B reads same data");

    fill_payload(box->b_payload, PAYLOAD_SIZE, 0xb22d0001U);
    box->b_checksum = checksum(box->b_payload, PAYLOAD_SIZE);
    __sync_synchronize();
    puts("VM B writes BAR2");
    write_doorbell(bar0, 0, 2);
    puts("VM B writes doorbell(target=A)");
    box->b_seq = B_TO_A_SEQ;
    __sync_synchronize();
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    setvbuf(stderr, NULL, _IONBF, 0);
    ensure_sysfs();

    volatile uint32_t *bar0 = map_resource("resource0", 0x1000);
    struct bar2_mailbox *bar2 = map_resource("resource2", BAR_SIZE);
    uint32_t peer_id = bar0[0];

    printf("vpci bar2 smoke peer_id=%u\n", peer_id);
    if (peer_id == 0) {
        peer0(bar0, bar2);
    } else if (peer_id == 1) {
        peer1(bar0, bar2);
    } else {
        fprintf(stderr, "vpci bar2 smoke failed: unexpected peer_id=%u\n", peer_id);
        return 1;
    }

    sync();
    for (;;) {
        pause();
    }
}
