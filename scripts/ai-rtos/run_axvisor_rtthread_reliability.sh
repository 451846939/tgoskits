#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
#
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

iterations="${1:-20}"
boot_timeout_s="${2:-180}"
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
result_dir="${repo_root}/tmp/ai-rtos/results/rtthread-reliability"

mkdir -p "${result_dir}"
AICP_RTTHREAD_RELIABILITY=1 \
  "${repo_root}/scripts/ai-rtos/run_axvisor_linux_rtthread_aicp.sh" \
  "${iterations}" ai "${boot_timeout_s}"

summary_source="${repo_root}/tmp/ai-rtos/results/axvisor-linux-rtthread/latest-summary.txt"
cp "${summary_source}" "${result_dir}/summary.txt"

grep -q 'AICP_RTTHREAD_RELIABILITY_SUMMARY passed=8 failed=0' \
  "${result_dir}/summary.txt"
grep -q 'AICP_LINUX_DONE ok=.*failed=0' "${result_dir}/summary.txt"

echo "[ai-rtos] PASS：RT-Thread Guest 可靠性结果位于 ${result_dir}"
