#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
source "${repo_root}/scripts/ai-rtos/lib/host_tools.sh"
source "${repo_root}/scripts/ai-rtos/lib/third_party_source_guard.sh"
source_dir="${repo_root}/apps/ai-rtos-demo/freertos"
kernel_dir="${FREERTOS_KERNEL_DIR:-${repo_root}/tmp/ai-rtos/FreeRTOS-Kernel}"
tcp_dir="${FREERTOS_PLUS_TCP_DIR:-${repo_root}/tmp/ai-rtos/FreeRTOS-Plus-TCP}"
kernel_revision="${FREERTOS_KERNEL_REVISION:-V11.2.0}"
tcp_revision="${FREERTOS_PLUS_TCP_REVISION:-V4.3.1}"
kernel_url="${FREERTOS_KERNEL_URL:-https://github.com/FreeRTOS/FreeRTOS-Kernel.git}"
tcp_url="${FREERTOS_PLUS_TCP_URL:-https://github.com/FreeRTOS/FreeRTOS-Plus-TCP.git}"
build_dir="${FREERTOS_BUILD_DIR:-${repo_root}/tmp/ai-rtos/build-freertos-aicp}"
cross_prefix="$(aicp_resolve_or_install_aarch64_none_elf \
  "${repo_root}" "14.3.rel1" CROSS_COMPILE)"
baseline="${AICP_FREERTOS_BASELINE:-OFF}"
stress="${AICP_FREERTOS_STRESS:-OFF}"

if [[ ! -d "${kernel_dir}/.git" ]]; then
  echo "[ai-rtos] Cloning FreeRTOS-Kernel revision=${kernel_revision}"
  git clone --depth 1 --branch "${kernel_revision}" "${kernel_url}" "${kernel_dir}"
fi
if [[ ! -d "${tcp_dir}/.git" ]]; then
  echo "[ai-rtos] Cloning FreeRTOS-Plus-TCP revision=${tcp_revision}"
  git clone --depth 1 --branch "${tcp_revision}" "${tcp_url}" "${tcp_dir}"
fi

verify_freertos_sources() {
  third_party_assert_git_source "FreeRTOS-Kernel" "${kernel_dir}" "${kernel_revision}"
  third_party_assert_git_source "FreeRTOS-Plus-TCP" "${tcp_dir}" "${tcp_revision}"
}

verify_freertos_on_exit() {
  local status=$?
  trap - EXIT
  if ! verify_freertos_sources; then
    exit 1
  fi
  exit "${status}"
}

verify_freertos_sources
trap verify_freertos_on_exit EXIT

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
trap - EXIT
verify_freertos_sources
