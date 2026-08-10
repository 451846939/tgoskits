// Copyright 2026 The TGOSKits Authors
//
// SPDX-License-Identifier: Apache-2.0

#include "simple_nn.h"

#include <math.h>

static float clampf(float value, float low, float high) {
    if (value < low) {
        return low;
    }
    if (value > high) {
        return high;
    }
    return value;
}

float aicp_target_trajectory(unsigned step) {
    float base = 0.52f + 0.28f * sinf((float)step * 0.055f);
    float pulse = (step % 80u >= 40u) ? 0.10f : -0.04f;
    return clampf(base + pulse, 0.05f, 0.95f);
}

float aicp_nn_infer_adaptation(float sensor, float trend, float load) {
    static const float w1[4][3] = {
        { 0.70f, -0.22f, 0.15f },
        { -0.35f, 0.91f, 0.08f },
        { 0.24f, 0.10f, -0.52f },
        { 0.44f, 0.28f, 0.36f },
    };
    static const float b1[4] = { 0.05f, -0.02f, 0.01f, 0.03f };
    static const float w2[4] = { 0.48f, -0.31f, 0.22f, 0.35f };
    float x[3] = { sensor, trend, load };
    float output = 0.45f;

    for (unsigned i = 0; i < 4; i++) {
        float hidden = b1[i];
        for (unsigned j = 0; j < 3; j++) {
            hidden += w1[i][j] * x[j];
        }
        output += w2[i] * tanhf(hidden);
    }
    return clampf(output, 0.05f, 0.95f);
}
