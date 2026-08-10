#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
#
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

iterations="${1:-20}"
mode="${2:-ai}"
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
source "${repo_root}/scripts/ai-rtos/lib/process.sh"
demo_dir="${repo_root}/apps/ai-rtos-demo"
build_dir="${RTTHREAD_BUILD_DIR:-${repo_root}/tmp/rtthread-aicp-build}"
gic_version="${RTTHREAD_GIC_VERSION:-2}"
ram_base="${RTTHREAD_RAM_BASE:-0x40000000}"
result_dir="${RTTHREAD_SMOKE_RESULT_DIR:-${repo_root}/tmp/ai-rtos/results/rtthread-aicp-smoke}"
log_file="${result_dir}/qemu.log"
client_log="${result_dir}/client.log"
csv_file="${result_dir}/client.csv"

mkdir -p "${result_dir}"
"${repo_root}/scripts/ai-rtos/build_rtthread_aicp_guest.sh"
make -C "${demo_dir}" all

cleanup() {
  aicp_cleanup_process_tree "${qemu_pid:-}"
}
trap cleanup EXIT

qemu-system-aarch64 \
  -M "virt,gic-version=${gic_version}" -cpu cortex-a53 -m 256M -smp 1 \
  -kernel "${build_dir}/rtthread.bin" -nographic -no-reboot \
  -netdev user,id=net0,hostfwd=tcp::18810-:8800 \
  -device virtio-net-device,netdev=net0,bus=virtio-mmio-bus.0 \
  >"${log_file}" 2>&1 &
qemu_pid=$!

deadline=$((SECONDS + 60))
while ((SECONDS < deadline)); do
  if grep -q "AICP_RTTHREAD_READY" "${log_file}" 2>/dev/null; then
    break
  fi
  if ! kill -0 "${qemu_pid}" 2>/dev/null; then
    tail -n 160 "${log_file}" >&2 || true
    exit 1
  fi
  sleep 1
done
grep -q "AICP_RTTHREAD_READY" "${log_file}"

"${demo_dir}/build/aicp_client" 127.0.0.1 18810 "${iterations}" \
  "${csv_file}" "${mode}" | tee "${client_log}"
grep -q "AICP_RTTHREAD_CONTROL" "${log_file}"

echo "[ai-rtos] PASS: RT-Thread 独立 QEMU AICP/TCP 闭环完成 ram_base=${ram_base} gic=v${gic_version}"
tail -n 1 "${client_log}"
