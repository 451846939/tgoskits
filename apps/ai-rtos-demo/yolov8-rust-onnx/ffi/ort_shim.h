// Copyright 2026 The TGOSKits Authors
//
// SPDX-License-Identifier: Apache-2.0

#ifndef TGOSKITS_AICP_YOLOV8_RUST_ORT_SHIM_H
#define TGOSKITS_AICP_YOLOV8_RUST_ORT_SHIM_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

struct aicp_ort_session;
struct aicp_ort_output;

struct aicp_ort_session *aicp_ort_create(
    const char *model_path,
    int threads,
    char *error,
    size_t error_capacity);

void aicp_ort_destroy(struct aicp_ort_session *session);

int aicp_ort_run(
    struct aicp_ort_session *session,
    float *input,
    size_t input_elements,
    const int64_t *input_shape,
    size_t input_rank,
    struct aicp_ort_output **output,
    const float **output_data,
    int64_t *output_shape,
    size_t output_shape_capacity,
    size_t *output_rank,
    size_t *output_elements,
    char *error,
    size_t error_capacity);

void aicp_ort_release_output(
    struct aicp_ort_session *session,
    struct aicp_ort_output *output);

unsigned char *aicp_image_load_rgb(
    const char *path,
    int *width,
    int *height,
    char *error,
    size_t error_capacity);

void aicp_image_free(unsigned char *data);

#ifdef __cplusplus
}
#endif

#endif
