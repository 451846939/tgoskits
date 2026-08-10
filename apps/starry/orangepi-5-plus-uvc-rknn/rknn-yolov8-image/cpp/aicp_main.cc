// Copyright 2026 The TGOSKits Authors
//
// Licensed under the Apache License, Version 2.0.

#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <string>
#include <vector>

#include "aicp_bridge.h"
#include "detection_validation.h"
#include "image_utils.h"
#include "yolov8.h"

struct Options {
    const char *model_path = "model/yolov8.rknn";
    const char *label_path = "model/coco_80_labels_list.txt";
    const char *image_path = NULL;
    const char *image_list_path = NULL;
    const char *aicp_host = "10.0.3.2";
    int aicp_port = 8800;
    int connect_timeout_ms = 1000;
    int min_confidence = 25;
    int target_class = 32;
    bool dry_run = false;
};

static void print_usage(const char *argv0)
{
    printf("Usage: %s [OPTIONS]\n", argv0);
    printf("  --model <PATH>                 RKNN model [default: model/yolov8.rknn]\n");
    printf("  --label <PATH>                 label file [default: model/coco_80_labels_list.txt]\n");
    printf("  --image <PATH>                 run one image and send one AICP control\n");
    printf("  --image-list <PATH>            run images from list and send controls\n");
    printf("  --aicp-host <IPv4>             RTOS AICP server [default: 10.0.3.2]\n");
    printf("  --aicp-port <PORT>             RTOS AICP port [default: 8800]\n");
    printf("  --connect-timeout-ms <MS>      TCP connect/read/write timeout [default: 1000]\n");
    printf("  --min-confidence <PCT>         detection threshold percentage [default: 25]\n");
    printf("  --target-class <ID>            COCO class id, -1 means best object [default: 32 sports ball]\n");
    printf("  --dry-run                      print mapped control without network send\n");
}

static bool parse_int_arg(const char *name, const char *value, int *out, int min_value, int max_value)
{
    if (value == NULL || value[0] == '\0') {
        printf("invalid value for %s\n", name);
        return false;
    }
    char *end = NULL;
    errno = 0;
    long parsed = strtol(value, &end, 10);
    if (errno != 0 || end == value || *end != '\0' || parsed < min_value || parsed > max_value) {
        printf("invalid value for %s: %s\n", name, value);
        return false;
    }
    *out = (int)parsed;
    return true;
}

static bool parse_args(int argc, char **argv, Options *options)
{
    for (int i = 1; i < argc; i++) {
        const char *arg = argv[i];
        const char *value = i + 1 < argc ? argv[i + 1] : NULL;
        if (strcmp(arg, "-h") == 0 || strcmp(arg, "--help") == 0) {
            print_usage(argv[0]);
            exit(0);
        } else if (strcmp(arg, "--model") == 0 && value != NULL) {
            options->model_path = value;
            i++;
        } else if (strcmp(arg, "--label") == 0 && value != NULL) {
            options->label_path = value;
            i++;
        } else if (strcmp(arg, "--image") == 0 && value != NULL) {
            options->image_path = value;
            i++;
        } else if (strcmp(arg, "--image-list") == 0 && value != NULL) {
            options->image_list_path = value;
            i++;
        } else if (strcmp(arg, "--aicp-host") == 0 && value != NULL) {
            options->aicp_host = value;
            i++;
        } else if (strcmp(arg, "--aicp-port") == 0 && value != NULL) {
            if (!parse_int_arg(arg, value, &options->aicp_port, 1, 65535)) return false;
            i++;
        } else if (strcmp(arg, "--connect-timeout-ms") == 0 && value != NULL) {
            if (!parse_int_arg(arg, value, &options->connect_timeout_ms, 1, 60000)) return false;
            i++;
        } else if (strcmp(arg, "--min-confidence") == 0 && value != NULL) {
            if (!parse_int_arg(arg, value, &options->min_confidence, 1, 99)) return false;
            i++;
        } else if (strcmp(arg, "--target-class") == 0 && value != NULL) {
            if (!parse_int_arg(arg, value, &options->target_class, -1, 10000)) return false;
            i++;
        } else if (strcmp(arg, "--dry-run") == 0) {
            options->dry_run = true;
        } else {
            printf("unknown or incomplete argument: %s\n", arg);
            return false;
        }
    }
    if ((options->image_path == NULL) == (options->image_list_path == NULL)) {
        printf("exactly one of --image or --image-list is required\n");
        return false;
    }
    return true;
}

static bool load_image_paths(const Options &options, std::vector<std::string> *paths)
{
    paths->clear();
    if (options.image_path != NULL) {
        paths->push_back(options.image_path);
        return true;
    }

    std::string error;
    std::vector<rknn_validation::ValidationImage> images;
    if (!rknn_validation::ReadImageListFile(options.image_list_path, &images, &error)) {
        printf("AICP_YOLO_FAIL stage=read_image_list reason=%s\n", error.c_str());
        return false;
    }
    for (size_t i = 0; i < images.size(); i++) {
        paths->push_back(images[i].path);
    }
    return !paths->empty();
}

static int run_one_image(const Options &options,
                         rknn_app_context_t *app_ctx,
                         const char *path,
                         AicpClient *client,
                         unsigned *ok,
                         unsigned *failed)
{
    image_buffer_t image;
    memset(&image, 0, sizeof(image));
    int ret = read_image(path, &image);
    if (ret != 0) {
        printf("AICP_YOLO_FAIL image=%s stage=read_image ret=%d\n", path, ret);
        (*failed)++;
        return ret;
    }

    object_detect_result_list results;
    memset(&results, 0, sizeof(results));
    ret = inference_yolov8_model_with_thresholds(
        app_ctx,
        &image,
        &results,
        (float)options.min_confidence / 100.0f,
        NMS_THRESH);
    if (ret != 0) {
        printf("AICP_YOLO_FAIL image=%s stage=inference ret=%d\n", path, ret);
        if (image.virt_addr != NULL) {
            free(image.virt_addr);
        }
        (*failed)++;
        return ret;
    }

    AicpControlMapping mapping;
    if (!aicp_map_detection_to_control(&results, image.width, image.height, options.target_class, &mapping)) {
        printf("AICP_YOLO_FAIL image=%s stage=map_control\n", path);
        if (image.virt_addr != NULL) {
            free(image.virt_addr);
        }
        (*failed)++;
        return -EINVAL;
    }

    printf("AICP_YOLO_RESULT image=%s detections=%d selected=%d cls=%d score=%.3f box=%d,%d,%d,%d target=%.4f kp=%.4f ki=%.4f kd=%.4f feed_forward=%.4f mode=%u\n",
           path,
           results.count,
           mapping.has_detection ? 1 : 0,
           mapping.cls_id,
           mapping.confidence,
           mapping.left,
           mapping.top,
           mapping.right,
           mapping.bottom,
           mapping.target,
           mapping.kp,
           mapping.ki,
           mapping.kd,
           mapping.feed_forward,
           mapping.mode);

    if (!options.dry_run) {
        uint64_t rtt_ns = 0;
        float measured = 0.0f;
        float error = 0.0f;
        float output = 0.0f;
        ret = aicp_client_send_control(client, &mapping, &rtt_ns, &measured, &error, &output);
        if (ret != 0) {
            printf("AICP_YOLO_FAIL image=%s stage=aicp_send ret=%d\n", path, ret);
            (*failed)++;
        } else {
            printf("AICP_YOLO_CONTROL image=%s rtt_ns=%llu measured=%.4f error=%.4f output=%.4f\n",
                   path,
                   (unsigned long long)rtt_ns,
                   measured,
                   error,
                   output);
            (*ok)++;
        }
    } else {
        (*ok)++;
    }

    if (image.virt_addr != NULL) {
        free(image.virt_addr);
    }
    return ret;
}

int main(int argc, char **argv)
{
    Options options;
    if (!parse_args(argc, argv, &options)) {
        print_usage(argv[0]);
        return 2;
    }

    std::vector<std::string> image_paths;
    if (!load_image_paths(options, &image_paths)) {
        return 1;
    }

    printf("AICP_YOLO_BEGIN model=%s label=%s images=%llu host=%s port=%d dry_run=%d target_class=%d\n",
           options.model_path,
           options.label_path,
           (unsigned long long)image_paths.size(),
           options.aicp_host,
           options.aicp_port,
           options.dry_run ? 1 : 0,
           options.target_class);

    int ret = init_post_process(options.label_path);
    if (ret != 0) {
        printf("AICP_YOLO_FAIL stage=init_post_process ret=%d\n", ret);
        return 1;
    }

    rknn_app_context_t app_ctx;
    memset(&app_ctx, 0, sizeof(app_ctx));
    ret = init_yolov8_model(options.model_path, &app_ctx);
    if (ret != 0) {
        printf("AICP_YOLO_FAIL stage=init_yolov8_model ret=%d\n", ret);
        deinit_post_process();
        return 1;
    }

    AicpClient client;
    memset(&client, 0, sizeof(client));
    client.fd = -1;
    if (!options.dry_run) {
        ret = aicp_client_connect(
            &client,
            options.aicp_host,
            (uint16_t)options.aicp_port,
            options.connect_timeout_ms);
        if (ret != 0) {
            printf("AICP_YOLO_FAIL stage=aicp_connect ret=%d host=%s port=%d\n",
                   ret,
                   options.aicp_host,
                   options.aicp_port);
            release_yolov8_model(&app_ctx);
            deinit_post_process();
            return 1;
        }
        ret = aicp_client_send_hello(&client, "yolov8-rknn-aicp");
        if (ret != 0) {
            printf("AICP_YOLO_FAIL stage=aicp_hello ret=%d\n", ret);
            aicp_client_close(&client);
            release_yolov8_model(&app_ctx);
            deinit_post_process();
            return 1;
        }
    }

    unsigned ok = 0;
    unsigned failed = 0;
    for (size_t i = 0; i < image_paths.size(); i++) {
        run_one_image(options, &app_ctx, image_paths[i].c_str(), &client, &ok, &failed);
    }

    aicp_client_close(&client);
    ret = release_yolov8_model(&app_ctx);
    deinit_post_process();

    printf("AICP_YOLO_DONE ok=%u failed=%u release_ret=%d\n", ok, failed, ret);
    return failed == 0 && ret == 0 ? 0 : 1;
}
