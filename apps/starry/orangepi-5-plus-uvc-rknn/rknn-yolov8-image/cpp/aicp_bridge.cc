// Copyright 2026 The TGOSKits Authors
//
// Licensed under the Apache License, Version 2.0.

#include "aicp_bridge.h"

#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <math.h>
#include <netinet/in.h>
#include <stdio.h>
#include <string.h>
#include <sys/select.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

#include "aicp_client.h"
#include "aicp_posix_stream.h"

static uint64_t monotonic_ns()
{
    struct timespec ts;
    if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0) {
        return 0;
    }
    return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
}

static uint64_t client_monotonic_ns(void *)
{
    return monotonic_ns();
}

static const struct aicp_client_ops client_ops = {
    client_monotonic_ns,
    NULL,
    NULL,
};

static float clampf_local(float value, float min_value, float max_value)
{
    if (value < min_value) {
        return min_value;
    }
    if (value > max_value) {
        return max_value;
    }
    return value;
}

static int set_io_timeout(int fd, int timeout_ms)
{
    struct timeval tv;
    tv.tv_sec = timeout_ms / 1000;
    tv.tv_usec = (timeout_ms % 1000) * 1000;
    if (setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof(tv)) != 0) {
        return -errno;
    }
    if (setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &tv, sizeof(tv)) != 0) {
        return -errno;
    }
    return 0;
}

static int connect_with_timeout(int fd, const struct sockaddr *addr, socklen_t addr_len, int timeout_ms)
{
    int flags = fcntl(fd, F_GETFL, 0);
    if (flags < 0) {
        return -errno;
    }
    if (fcntl(fd, F_SETFL, flags | O_NONBLOCK) != 0) {
        return -errno;
    }

    int ret = connect(fd, addr, addr_len);
    if (ret != 0 && errno != EINPROGRESS) {
        int saved = errno;
        (void)fcntl(fd, F_SETFL, flags);
        return -saved;
    }
    if (ret != 0) {
        fd_set wfds;
        FD_ZERO(&wfds);
        FD_SET(fd, &wfds);
        struct timeval tv;
        tv.tv_sec = timeout_ms / 1000;
        tv.tv_usec = (timeout_ms % 1000) * 1000;
        ret = select(fd + 1, NULL, &wfds, NULL, &tv);
        if (ret == 0) {
            (void)fcntl(fd, F_SETFL, flags);
            return -ETIMEDOUT;
        }
        if (ret < 0) {
            int saved = errno;
            (void)fcntl(fd, F_SETFL, flags);
            return -saved;
        }
        int so_error = 0;
        socklen_t len = sizeof(so_error);
        if (getsockopt(fd, SOL_SOCKET, SO_ERROR, &so_error, &len) != 0) {
            int saved = errno;
            (void)fcntl(fd, F_SETFL, flags);
            return -saved;
        }
        if (so_error != 0) {
            (void)fcntl(fd, F_SETFL, flags);
            return -so_error;
        }
    }

    if (fcntl(fd, F_SETFL, flags) != 0) {
        return -errno;
    }
    return set_io_timeout(fd, timeout_ms);
}

bool aicp_map_detection_to_control(const object_detect_result_list *results,
                                   int image_width,
                                   int image_height,
                                   int target_class,
                                   AicpControlMapping *mapping)
{
    if (mapping == NULL || image_width <= 0 || image_height <= 0) {
        return false;
    }

    memset(mapping, 0, sizeof(*mapping));
    mapping->target = 0.0f;
    mapping->kp = 0.42f;
    mapping->ki = 0.02f;
    mapping->kd = 0.01f;
    mapping->feed_forward = 0.0f;
    mapping->mode = 2;
    mapping->cls_id = -1;
    mapping->confidence = 0.0f;

    if (results == NULL || results->count <= 0) {
        return true;
    }

    const object_detect_result *best = NULL;
    for (int i = 0; i < results->count; i++) {
        const object_detect_result *det = &results->results[i];
        if (target_class >= 0 && det->cls_id != target_class) {
            continue;
        }
        if (best == NULL || det->prop > best->prop) {
            best = det;
        }
    }
    if (best == NULL) {
        return true;
    }

    const float left = clampf_local((float)best->box.left, 0.0f, (float)image_width);
    const float right = clampf_local((float)best->box.right, 0.0f, (float)image_width);
    const float top = clampf_local((float)best->box.top, 0.0f, (float)image_height);
    const float bottom = clampf_local((float)best->box.bottom, 0.0f, (float)image_height);
    const float width = clampf_local(right - left, 1.0f, (float)image_width);
    const float height = clampf_local(bottom - top, 1.0f, (float)image_height);
    const float center_x = left + width * 0.5f;
    const float center_y = top + height * 0.5f;
    const float x_error = center_x / (float)image_width - 0.5f;
    const float y_error = 0.5f - center_y / (float)image_height;
    const float area_ratio = clampf_local((width * height) / ((float)image_width * (float)image_height), 0.0f, 1.0f);
    const float conf = clampf_local(best->prop, 0.0f, 1.0f);

    mapping->target = clampf_local(x_error * 2.0f, -1.0f, 1.0f);
    mapping->kp = 0.45f + 0.35f * conf;
    mapping->ki = 0.02f + 0.10f * area_ratio;
    mapping->kd = 0.02f + 0.10f * fabsf(x_error);
    mapping->feed_forward = clampf_local(y_error * 0.35f, -0.25f, 0.25f);
    mapping->mode = 3;
    mapping->cls_id = best->cls_id;
    mapping->confidence = conf;
    mapping->left = best->box.left;
    mapping->top = best->box.top;
    mapping->right = best->box.right;
    mapping->bottom = best->box.bottom;
    mapping->has_detection = true;
    return true;
}

int aicp_client_connect(AicpClient *client, const char *host, uint16_t port, int timeout_ms)
{
    if (client == NULL || host == NULL) {
        return -EINVAL;
    }
    memset(client, 0, sizeof(*client));
    client->fd = -1;
    client->seq = 1;

    int fd = socket(AF_INET, SOCK_STREAM, 0);
    if (fd < 0) {
        return -errno;
    }

    struct sockaddr_in addr;
    memset(&addr, 0, sizeof(addr));
    addr.sin_family = AF_INET;
    addr.sin_port = htons(port);
    if (inet_pton(AF_INET, host, &addr.sin_addr) != 1) {
        int saved = errno != 0 ? errno : EINVAL;
        close(fd);
        return -saved;
    }

    int ret = connect_with_timeout(fd, (struct sockaddr *)&addr, sizeof(addr), timeout_ms);
    if (ret != 0) {
        close(fd);
        return ret;
    }
    client->fd = fd;
    return 0;
}

void aicp_client_close(AicpClient *client)
{
    if (client != NULL && client->fd >= 0) {
        close(client->fd);
        client->fd = -1;
    }
}

int aicp_client_send_hello(AicpClient *client, const char *role)
{
    if (client == NULL || client->fd < 0) {
        return -ENOTCONN;
    }
    char payload[160];
    snprintf(payload,
             sizeof(payload),
             "{\"role\":\"%s\",\"model\":\"yolov8-rknn\",\"cap\":\"control,status\"}",
             role != NULL ? role : "yolov8-aicp");
    struct aicp_posix_stream stream;
    aicp_posix_stream_init(&stream, client->fd);
    return aicp_client_session_send_hello(
        &stream.stream,
        &client->seq,
        payload,
        (uint32_t)strlen(payload) + 1,
        &client_ops);
}

int aicp_client_send_control(AicpClient *client,
                             const AicpControlMapping *mapping,
                             uint64_t *rtt_ns,
                             float *measured,
                             float *error,
                             float *control_output)
{
    if (client == NULL || client->fd < 0 || mapping == NULL) {
        return -EINVAL;
    }

    struct aicp_control_payload control;
    control.target = mapping->target;
    control.kp = mapping->kp;
    control.ki = mapping->ki;
    control.kd = mapping->kd;
    control.feed_forward = mapping->feed_forward;
    control.mode = mapping->mode;

    struct aicp_status_payload status;
    struct aicp_posix_stream stream;
    aicp_posix_stream_init(&stream, client->fd);
    int ret = aicp_client_session_transact_control(
        &stream.stream,
        &client->seq,
        &control,
        &status,
        rtt_ns,
        &client_ops);
    if (ret != 0) {
        return ret;
    }
    if (measured != NULL) {
        *measured = status.measured;
    }
    if (error != NULL) {
        *error = status.error;
    }
    if (control_output != NULL) {
        *control_output = status.control_output;
    }
    return 0;
}
