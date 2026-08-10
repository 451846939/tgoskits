#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
#
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
source "${repo_root}/scripts/ai-rtos/lib/third_party_source_guard.sh"

checked=0
zephyr_base="${ZEPHYR_BASE:-${repo_root}/tmp/zephyrproject/zephyr}"
if [[ -d "${zephyr_base}/.git" ]]; then
  third_party_assert_git_source \
    "Zephyr" "${zephyr_base}" "${ZEPHYR_REQUIRED_REF:-v4.4.0}"
  third_party_assert_nested_git_clean \
    "Zephyr workspace" "$(cd "${zephyr_base}/.." && pwd)"
  checked=$((checked + 1))
fi

rtthread_revision="${RTTHREAD_REVISION:-v5.2.1}"
rtthread_source="${RTTHREAD_SOURCE_DIR:-${repo_root}/tmp/rt-thread-${rtthread_revision}}"
if [[ -d "${rtthread_source}/.git" ]]; then
  third_party_assert_git_source \
    "RT-Thread" "${rtthread_source}" "${rtthread_revision}"
  checked=$((checked + 1))
fi

freertos_kernel_source="${FREERTOS_KERNEL_DIR:-${FREERTOS_SOURCE_DIR:-${repo_root}/tmp/ai-rtos/FreeRTOS-Kernel}}"
if [[ -d "${freertos_kernel_source}/.git" ]]; then
  third_party_assert_git_source \
    "FreeRTOS-Kernel" "${freertos_kernel_source}" "${FREERTOS_KERNEL_REQUIRED_REF:-${FREERTOS_REQUIRED_REF:-}}"
  checked=$((checked + 1))
fi

freertos_tcp_source="${FREERTOS_PLUS_TCP_DIR:-${repo_root}/tmp/ai-rtos/FreeRTOS-Plus-TCP}"
if [[ -d "${freertos_tcp_source}/.git" ]]; then
  third_party_assert_git_source \
    "FreeRTOS+TCP" "${freertos_tcp_source}" "${FREERTOS_PLUS_TCP_REQUIRED_REF:-}"
  checked=$((checked + 1))
fi

if (( checked == 0 )); then
  echo "[ai-rtos] no configured third-party source worktree was found"
else
  echo "[ai-rtos] PASS: checked ${checked} third-party source workspace(s)"
fi
