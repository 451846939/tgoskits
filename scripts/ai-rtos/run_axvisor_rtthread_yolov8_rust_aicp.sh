#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
#
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

export AICP_RTTHREAD_CLIENT_PROFILE=yolov8-rust
exec "${repo_root}/scripts/ai-rtos/run_axvisor_linux_rtthread_aicp.sh" 3 ai "${1:-420}"
