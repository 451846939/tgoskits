#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
source "${repo_root}/scripts/ai-rtos/lib/host_tools.sh"
source_dir="${repo_root}/apps/ai-rtos-demo/freertos"
kernel_dir="${FREERTOS_KERNEL_DIR:-${repo_root}/tmp/ai-rtos/FreeRTOS-Kernel}"
tcp_dir="${FREERTOS_PLUS_TCP_DIR:-${repo_root}/tmp/ai-rtos/FreeRTOS-Plus-TCP}"
build_dir="${FREERTOS_BUILD_DIR:-${repo_root}/tmp/ai-rtos/build-freertos-aicp}"
cross_prefix="$(aicp_resolve_or_install_aarch64_none_elf \
  "${repo_root}" "14.3.rel1" CROSS_COMPILE)"
baseline="${AICP_FREERTOS_BASELINE:-OFF}"
stress="${AICP_FREERTOS_STRESS:-OFF}"

test -f "${kernel_dir}/tasks.c" || {
  echo "ERROR: 未找到 FreeRTOS-Kernel：${kernel_dir}" >&2
  exit 1
}
test -f "${tcp_dir}/source/FreeRTOS_IP.c" || {
  echo "ERROR: 未找到 FreeRTOS-Plus-TCP：${tcp_dir}" >&2
  exit 1
}

cmake --fresh -S "${source_dir}" -B "${build_dir}" -G Ninja \
  -DCMAKE_SYSTEM_NAME=Generic \
  -DCMAKE_C_COMPILER="${cross_prefix}gcc" \
  -DCMAKE_ASM_COMPILER="${cross_prefix}gcc" \
  -DCMAKE_OBJCOPY="${cross_prefix}objcopy" \
  -DCMAKE_TRY_COMPILE_TARGET_TYPE=STATIC_LIBRARY \
  -DFREERTOS_KERNEL_DIR="${kernel_dir}" \
  -DFREERTOS_PLUS_TCP_DIR="${tcp_dir}" \
  -DAICP_FREERTOS_BASELINE="${baseline}" \
  -DAICP_FREERTOS_STRESS="${stress}"
cmake --build "${build_dir}" --verbose

entry="$(${cross_prefix}nm -n "${build_dir}/aicp-freertos.elf" | awk '$3 == "_boot" && !found { print "0x" $1; found = 1 }')"
test -n "${entry}" || {
  echo "ERROR: 无法从 ELF 获取 _boot 入口" >&2
  exit 1
}

echo "AICP_FREERTOS_BUILD_OK"
echo "elf=${build_dir}/aicp-freertos.elf"
echo "bin=${build_dir}/aicp-freertos.bin"
echo "entry=${entry}"
echo "baseline=${baseline}"
echo "stress=${stress}"
