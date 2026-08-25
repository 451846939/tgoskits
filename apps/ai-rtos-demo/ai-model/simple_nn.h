// Copyright 2026 The TGOSKits Authors
//
// SPDX-License-Identifier: Apache-2.0

#ifndef AICP_SIMPLE_NN_H
#define AICP_SIMPLE_NN_H

float aicp_target_trajectory(unsigned step);
float aicp_nn_infer_adaptation(float sensor, float trend, float load);

#endif
