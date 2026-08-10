// Copyright 2026 The TGOSKits Authors
//
// Licensed under the Apache License, Version 2.0.

#ifndef RKNN_YOLOV8_IMAGE_AICP_BRIDGE_H_
#define RKNN_YOLOV8_IMAGE_AICP_BRIDGE_H_

#include <stdint.h>

#include "yolov8.h"

struct AicpControlMapping {
    float target;
    float kp;
    float ki;
    float kd;
    float feed_forward;
    uint32_t mode;
    int cls_id;
    float confidence;
    int left;
    int top;
    int right;
    int bottom;
    bool has_detection;
};

struct AicpClient {
    int fd;
    uint32_t seq;
};

bool aicp_map_detection_to_control(const object_detect_result_list *results,
                                   int image_width,
                                   int image_height,
                                   int target_class,
                                   AicpControlMapping *mapping);
int aicp_client_connect(AicpClient *client, const char *host, uint16_t port, int timeout_ms);
void aicp_client_close(AicpClient *client);
int aicp_client_send_hello(AicpClient *client, const char *role);
int aicp_client_send_control(AicpClient *client,
                             const AicpControlMapping *mapping,
                             uint64_t *rtt_ns,
                             float *measured,
                             float *error,
                             float *control_output);

#endif  // RKNN_YOLOV8_IMAGE_AICP_BRIDGE_H_
