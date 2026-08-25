// Copyright 2026 The TGOSKits Authors
//
// SPDX-License-Identifier: Apache-2.0

#include "ort_shim.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <onnxruntime_c_api.h>

#define STB_IMAGE_IMPLEMENTATION
#include "stb_image.h"

struct aicp_ort_session {
    const OrtApi *api;
    OrtEnv *env;
    OrtSessionOptions *options;
    OrtSession *session;
    OrtAllocator *allocator;
    OrtMemoryInfo *memory_info;
    char *input_name;
    char *output_name;
};

struct aicp_ort_output {
    OrtValue *value;
};

static void set_error(char *error, size_t capacity, const char *message)
{
    if (error == NULL || capacity == 0) {
        return;
    }
    snprintf(error, capacity, "%s", message != NULL ? message : "unknown error");
}

static int check_status(
    const OrtApi *api,
    OrtStatus *status,
    char *error,
    size_t error_capacity)
{
    if (status == NULL) {
        return 0;
    }
    set_error(error, error_capacity, api->GetErrorMessage(status));
    api->ReleaseStatus(status);
    return -1;
}

static void cleanup_session(struct aicp_ort_session *session)
{
    if (session == NULL || session->api == NULL) {
        free(session);
        return;
    }
    if (session->input_name != NULL && session->allocator != NULL) {
        OrtStatus *status = session->api->AllocatorFree(session->allocator, session->input_name);
        if (status != NULL) {
            session->api->ReleaseStatus(status);
        }
    }
    if (session->output_name != NULL && session->allocator != NULL) {
        OrtStatus *status = session->api->AllocatorFree(session->allocator, session->output_name);
        if (status != NULL) {
            session->api->ReleaseStatus(status);
        }
    }
    if (session->memory_info != NULL) {
        session->api->ReleaseMemoryInfo(session->memory_info);
    }
    if (session->session != NULL) {
        session->api->ReleaseSession(session->session);
    }
    if (session->options != NULL) {
        session->api->ReleaseSessionOptions(session->options);
    }
    if (session->env != NULL) {
        session->api->ReleaseEnv(session->env);
    }
    free(session);
}

struct aicp_ort_session *aicp_ort_create(
    const char *model_path,
    int threads,
    char *error,
    size_t error_capacity)
{
    const OrtApiBase *base = OrtGetApiBase();
    const OrtApi *api = base != NULL ? base->GetApi(ORT_API_VERSION) : NULL;
    if (api == NULL) {
        set_error(error, error_capacity, "OrtGetApi failed");
        return NULL;
    }

    struct aicp_ort_session *out = calloc(1, sizeof(*out));
    if (out == NULL) {
        set_error(error, error_capacity, "out of memory");
        return NULL;
    }
    out->api = api;

    if (check_status(api, api->CreateEnv(ORT_LOGGING_LEVEL_WARNING,
                                         "aicp-yolov8-rust-onnx", &out->env),
                     error, error_capacity) != 0 ||
        check_status(api, api->CreateSessionOptions(&out->options),
                     error, error_capacity) != 0 ||
        check_status(api, api->SetIntraOpNumThreads(out->options, threads),
                     error, error_capacity) != 0 ||
        check_status(api, api->SetSessionGraphOptimizationLevel(
                         out->options, ORT_ENABLE_EXTENDED),
                     error, error_capacity) != 0 ||
        check_status(api, api->CreateSession(out->env, model_path, out->options,
                                             &out->session),
                     error, error_capacity) != 0 ||
        check_status(api, api->GetAllocatorWithDefaultOptions(&out->allocator),
                     error, error_capacity) != 0 ||
        check_status(api, api->SessionGetInputName(out->session, 0, out->allocator,
                                                   &out->input_name),
                     error, error_capacity) != 0 ||
        check_status(api, api->SessionGetOutputName(out->session, 0, out->allocator,
                                                    &out->output_name),
                     error, error_capacity) != 0 ||
        check_status(api, api->CreateCpuMemoryInfo(OrtArenaAllocator,
                                                   OrtMemTypeDefault,
                                                   &out->memory_info),
                     error, error_capacity) != 0) {
        cleanup_session(out);
        return NULL;
    }
    return out;
}

void aicp_ort_destroy(struct aicp_ort_session *session)
{
    cleanup_session(session);
}

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
    size_t error_capacity)
{
    if (session == NULL || input == NULL || output == NULL || output_data == NULL ||
        output_shape == NULL || output_rank == NULL || output_elements == NULL) {
        set_error(error, error_capacity, "invalid aicp_ort_run argument");
        return -1;
    }

    const OrtApi *api = session->api;
    OrtValue *input_value = NULL;
    OrtValue *result_value = NULL;
    OrtTensorTypeAndShapeInfo *shape_info = NULL;
    struct aicp_ort_output *result = NULL;
    void *data = NULL;
    enum ONNXTensorElementDataType element_type = ONNX_TENSOR_ELEMENT_DATA_TYPE_UNDEFINED;
    size_t rank = 0;
    size_t elements = 0;
    const char *input_names[] = {session->input_name};
    const char *output_names[] = {session->output_name};

    if (check_status(api, api->CreateTensorWithDataAsOrtValue(
                         session->memory_info, input, input_elements * sizeof(float),
                         input_shape, input_rank,
                         ONNX_TENSOR_ELEMENT_DATA_TYPE_FLOAT, &input_value),
                     error, error_capacity) != 0 ||
        check_status(api, api->Run(session->session, NULL, input_names,
                                   (const OrtValue *const *)&input_value, 1,
                                   output_names, 1, &result_value),
                     error, error_capacity) != 0 ||
        check_status(api, api->GetTensorTypeAndShape(result_value, &shape_info),
                     error, error_capacity) != 0 ||
        check_status(api, api->GetTensorElementType(shape_info, &element_type),
                     error, error_capacity) != 0 ||
        check_status(api, api->GetDimensionsCount(shape_info, &rank),
                     error, error_capacity) != 0) {
        goto fail;
    }
    if (element_type != ONNX_TENSOR_ELEMENT_DATA_TYPE_FLOAT) {
        set_error(error, error_capacity, "ONNX output tensor is not float32");
        goto fail;
    }
    if (rank == 0 || rank > output_shape_capacity) {
        set_error(error, error_capacity, "unsupported ONNX output rank");
        goto fail;
    }
    if (check_status(api, api->GetDimensions(shape_info, output_shape, rank),
                     error, error_capacity) != 0 ||
        check_status(api, api->GetTensorShapeElementCount(shape_info, &elements),
                     error, error_capacity) != 0 ||
        check_status(api, api->GetTensorMutableData(result_value, &data),
                     error, error_capacity) != 0) {
        goto fail;
    }

    result = calloc(1, sizeof(*result));
    if (result == NULL) {
        set_error(error, error_capacity, "out of memory");
        goto fail;
    }
    result->value = result_value;
    result_value = NULL;
    *output = result;
    *output_data = (const float *)data;
    *output_rank = rank;
    *output_elements = elements;
    api->ReleaseTensorTypeAndShapeInfo(shape_info);
    api->ReleaseValue(input_value);
    return 0;

fail:
    if (shape_info != NULL) {
        api->ReleaseTensorTypeAndShapeInfo(shape_info);
    }
    if (result_value != NULL) {
        api->ReleaseValue(result_value);
    }
    if (input_value != NULL) {
        api->ReleaseValue(input_value);
    }
    free(result);
    return -1;
}

void aicp_ort_release_output(
    struct aicp_ort_session *session,
    struct aicp_ort_output *output)
{
    if (session == NULL || output == NULL) {
        return;
    }
    if (output->value != NULL) {
        session->api->ReleaseValue(output->value);
    }
    free(output);
}

unsigned char *aicp_image_load_rgb(
    const char *path,
    int *width,
    int *height,
    char *error,
    size_t error_capacity)
{
    int channels = 0;
    unsigned char *data = stbi_load(path, width, height, &channels, 3);
    if (data == NULL) {
        set_error(error, error_capacity,
                  stbi_failure_reason() != NULL ? stbi_failure_reason()
                                                : "stbi_load failed");
    }
    return data;
}

void aicp_image_free(unsigned char *data)
{
    stbi_image_free(data);
}
