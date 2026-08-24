// Copyright 2026 The TGOSKits Authors
//
// SPDX-License-Identifier: Apache-2.0

#include "control_policy.h"

#include "simple_nn.h"

#include <math.h>

struct aicp_control_payload aicp_control_policy(unsigned step, bool ai_mode) {
    const float sensor = sinf((float)step * 0.07f);
    const float trend = cosf((float)step * 0.03f);
    const float load = (float)(step % 17u) / 16.0f;
    const float adaptation =
        ai_mode ? aicp_nn_infer_adaptation(sensor, trend, load) : 0.0f;

    return (struct aicp_control_payload){
        .target = aicp_target_trajectory(step),
        .kp = ai_mode ? 0.58f + 0.22f * adaptation : 0.45f,
        .ki = ai_mode ? 0.06f + 0.04f * load : 0.03f,
        .kd = ai_mode ? 0.02f : 0.00f,
        .feed_forward = ai_mode ? 0.08f * load + 0.04f * trend : 0.0f,
        .mode = ai_mode ? 1u : 0u,
    };
}
