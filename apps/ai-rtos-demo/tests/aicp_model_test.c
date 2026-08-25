// Copyright 2026 The TGOSKits Authors
//
// SPDX-License-Identifier: Apache-2.0

#include "control_policy.h"
#include "simple_nn.h"

#include <math.h>
#include <stdio.h>

static int test_inference_stays_within_the_control_range(void) {
    const float output = aicp_nn_infer_adaptation(1.0f, -1.0f, 0.75f);

    if (output < 0.05f || output > 0.95f) {
        fprintf(stderr, "inference result is outside the control range: %.6f\n", output);
        return 1;
    }
    return 0;
}

static int test_ai_policy_changes_the_adaptive_parameters(void) {
    const struct aicp_control_payload fixed = aicp_control_policy(7, false);
    const struct aicp_control_payload ai = aicp_control_policy(7, true);

    if (fixed.mode != 0 || ai.mode != 1 || fixed.target != ai.target) {
        fprintf(stderr, "policy mode or common target is incorrect\n");
        return 1;
    }
    if (fabsf(ai.kp - fixed.kp) < 0.0001f || fabsf(ai.ki - fixed.ki) < 0.0001f ||
        fabsf(ai.feed_forward - fixed.feed_forward) < 0.0001f) {
        fprintf(stderr, "AI policy did not change adaptive control parameters\n");
        return 1;
    }
    return 0;
}

int main(void) {
    int failed = 0;

    failed |= test_inference_stays_within_the_control_range();
    failed |= test_ai_policy_changes_the_adaptive_parameters();
    if (failed != 0) {
        return 1;
    }
    puts("AICP model tests passed: 2");
    return 0;
}
