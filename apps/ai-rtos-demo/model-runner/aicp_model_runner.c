// Copyright 2026 The TGOSKits Authors
//
// SPDX-License-Identifier: Apache-2.0

#define _POSIX_C_SOURCE 200809L

#include "control_policy.h"

#include <errno.h>
#include <math.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

struct plant_state {
    float measured;
    float integral;
    float last_error;
    float control_output;
};

static float clampf_local(float value, float low, float high) {
    if (value < low) {
        return low;
    }
    if (value > high) {
        return high;
    }
    return value;
}

static uint64_t now_ns(void) {
    struct timespec ts;
    if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0) {
        return 0;
    }
    return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
}

static void plant_step(
    struct plant_state *state,
    struct aicp_control_payload control) {
    float error = control.target - state->measured;
    state->integral = clampf_local(state->integral + error * 0.02f, -1.0f, 1.0f);
    float derivative = (error - state->last_error) / 0.02f;
    state->last_error = error;

    float raw = control.kp * error
        + control.ki * state->integral
        + control.kd * derivative
        + control.feed_forward;
    state->control_output = clampf_local(raw, -1.0f, 1.0f);
    state->measured += 0.18f * (state->control_output - state->measured) + 0.04f * error;
}

static void usage(const char *argv0) {
    fprintf(stderr,
        "usage: %s [--steps N] [--mode ai|fixed] [--csv PATH]\n"
        "       default: --steps 24 --mode ai\n",
        argv0);
}

static int parse_unsigned(const char *text, unsigned *out) {
    errno = 0;
    char *end = NULL;
    unsigned long value = strtoul(text, &end, 10);
    if (errno != 0 || end == text || *end != '\0' || value == 0 || value > 1000000ul) {
        return -1;
    }
    *out = (unsigned)value;
    return 0;
}

int main(int argc, char **argv) {
    unsigned steps = 24;
    bool ai_mode = true;
    const char *csv_path = NULL;

    for (int i = 1; i < argc; i++) {
        if (strcmp(argv[i], "--steps") == 0 && i + 1 < argc) {
            if (parse_unsigned(argv[++i], &steps) != 0) {
                usage(argv[0]);
                return 2;
            }
        } else if (strcmp(argv[i], "--mode") == 0 && i + 1 < argc) {
            const char *mode = argv[++i];
            if (strcmp(mode, "ai") == 0) {
                ai_mode = true;
            } else if (strcmp(mode, "fixed") == 0) {
                ai_mode = false;
            } else {
                usage(argv[0]);
                return 2;
            }
        } else if (strcmp(argv[i], "--csv") == 0 && i + 1 < argc) {
            csv_path = argv[++i];
        } else if (strcmp(argv[i], "--help") == 0) {
            usage(argv[0]);
            return 0;
        } else {
            usage(argv[0]);
            return 2;
        }
    }

    FILE *csv = NULL;
    if (csv_path != NULL) {
        csv = fopen(csv_path, "w");
        if (csv == NULL) {
            perror("fopen csv");
            return 1;
        }
        fprintf(csv,
            "step,mode,target,kp,ki,kd,feed_forward,measured,control_output,error,infer_ns\n");
    }

    struct plant_state state = { 0 };
    double abs_error_sum = 0.0;
    uint64_t infer_sum = 0;
    uint64_t infer_max = 0;

    printf("AICP_MODEL_RUNNER_BEGIN mode=%s steps=%u\n", ai_mode ? "ai" : "fixed", steps);
    for (unsigned step = 0; step < steps; step++) {
        const uint64_t infer_begin_ns = ai_mode ? now_ns() : 0;
        const struct aicp_control_payload control =
            aicp_control_policy(step, ai_mode);
        const uint64_t infer_end_ns = ai_mode ? now_ns() : 0;
        const uint64_t infer_ns =
            infer_end_ns > infer_begin_ns ? infer_end_ns - infer_begin_ns : 0;
        plant_step(&state, control);
        float error = control.target - state.measured;
        abs_error_sum += fabsf(error);
        infer_sum += infer_ns;
        if (infer_ns > infer_max) {
            infer_max = infer_ns;
        }

        printf("AICP_MODEL_STEP step=%u mode=%u target=%.4f kp=%.4f ki=%.4f kd=%.4f "
               "ff=%.4f measured=%.4f output=%.4f error=%.4f infer_ns=%llu\n",
            step,
            control.mode,
            control.target,
            control.kp,
            control.ki,
            control.kd,
            control.feed_forward,
            state.measured,
            state.control_output,
            error,
            (unsigned long long)infer_ns);

        if (csv != NULL) {
            fprintf(csv, "%u,%u,%.6f,%.6f,%.6f,%.6f,%.6f,%.6f,%.6f,%.6f,%llu\n",
                step,
                control.mode,
                control.target,
                control.kp,
                control.ki,
                control.kd,
                control.feed_forward,
                state.measured,
                state.control_output,
                error,
                (unsigned long long)infer_ns);
        }
    }

    if (csv != NULL) {
        fclose(csv);
    }

    printf("AICP_MODEL_RUNNER_DONE mode=%s steps=%u avg_abs_error=%.6f "
           "avg_infer_ns=%llu max_infer_ns=%llu\n",
        ai_mode ? "ai" : "fixed",
        steps,
        abs_error_sum / (double)steps,
        (unsigned long long)(infer_sum / steps),
        (unsigned long long)infer_max);
    return 0;
}
