// Copyright 2026 The TGOSKits Authors
//
// SPDX-License-Identifier: Apache-2.0

#ifndef TGOSKITS_AI_RTOS_DEMO_AICP_POSIX_STREAM_H
#define TGOSKITS_AI_RTOS_DEMO_AICP_POSIX_STREAM_H

#include "aicp_stream.h"

#include <errno.h>
#include <unistd.h>

#ifdef __cplusplus
extern "C" {
#endif

struct aicp_posix_stream {
    struct aicp_stream stream;
    int fd;
};

static inline ptrdiff_t aicp_posix_stream_read(
    void *context,
    void *buffer,
    size_t length) {
    struct aicp_posix_stream *posix = (struct aicp_posix_stream *)context;
    for (;;) {
        const ssize_t result = read(posix->fd, buffer, length);
        if (result >= 0) {
            return (ptrdiff_t)result;
        }
        if (errno != EINTR) {
            return (ptrdiff_t)-errno;
        }
    }
}

static inline ptrdiff_t aicp_posix_stream_write(
    void *context,
    const void *buffer,
    size_t length) {
    struct aicp_posix_stream *posix = (struct aicp_posix_stream *)context;
    for (;;) {
        const ssize_t result = write(posix->fd, buffer, length);
        if (result >= 0) {
            return (ptrdiff_t)result;
        }
        if (errno != EINTR) {
            return (ptrdiff_t)-errno;
        }
    }
}

static inline void aicp_posix_stream_init(
    struct aicp_posix_stream *posix,
    int fd) {
    posix->fd = fd;
    posix->stream.read = aicp_posix_stream_read;
    posix->stream.write = aicp_posix_stream_write;
    posix->stream.context = posix;
}

static inline int aicp_posix_read_full(
    int fd,
    void *buffer,
    size_t length) {
    struct aicp_posix_stream posix;

    aicp_posix_stream_init(&posix, fd);
    return aicp_stream_read_full(&posix.stream, buffer, length);
}

static inline int aicp_posix_write_full(
    int fd,
    const void *buffer,
    size_t length) {
    struct aicp_posix_stream posix;

    aicp_posix_stream_init(&posix, fd);
    return aicp_stream_write_full(&posix.stream, buffer, length);
}

static inline int aicp_posix_send_frame(
    int fd,
    struct aicp_header header,
    const void *payload) {
    struct aicp_posix_stream posix;

    aicp_posix_stream_init(&posix, fd);
    return aicp_stream_send_frame(&posix.stream, header, payload);
}

static inline int aicp_posix_recv_frame(
    int fd,
    struct aicp_header *header,
    void *payload,
    size_t capacity) {
    struct aicp_posix_stream posix;

    aicp_posix_stream_init(&posix, fd);
    return aicp_stream_recv_frame(&posix.stream, header, payload, capacity);
}

#ifdef __cplusplus
}
#endif

#endif
