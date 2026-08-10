#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
#
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  scripts/ai-rtos/run_zephyr_aicp_smoke.sh [iterations] [boot_timeout_seconds]

Runs the repository-owned Zephyr AICP application on stock Zephyr under QEMU,
then sends HELLO and CONTROL messages through TCP/IP. The third-party Zephyr
source tree must remain clean before and after the test.

Environment:
  ZEPHYR_BASE       official Zephyr Git worktree (required)
  ZEPHYR_ELF        guest ELF path
  QEMU              qemu-system-aarch64 path
  AICP_HOST_PORT    host-forwarded TCP port (default: 18800)
  AICP_RESULT_DIR   output directory
EOF
}

if [[ $# -gt 2 || "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

iterations="${1:-8}"
boot_timeout_s="${2:-30}"
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
source "${repo_root}/scripts/ai-rtos/lib/process.sh"
source "${repo_root}/scripts/ai-rtos/lib/markers.sh"
demo_dir="${repo_root}/apps/ai-rtos-demo"
host_port="${AICP_HOST_PORT:-18800}"
result_dir="${AICP_RESULT_DIR:-${repo_root}/tmp/ai-rtos/results/zephyr-aicp-smoke}"
zephyr_elf="${ZEPHYR_ELF:-${repo_root}/tmp/zephyrproject/build-aicp-network-v4.4/zephyr/zephyr.elf}"
qemu_bin="${QEMU:-qemu-system-aarch64}"
client="${demo_dir}/build/aicp_client"
guest_log="${result_dir}/zephyr-qemu.log"
client_log="${result_dir}/aicp-client.log"
csv_file="${result_dir}/aicp-results.csv"
qemu_pid=""

check_clean_zephyr() {
  if [[ -z "${ZEPHYR_BASE:-}" || ! -d "${ZEPHYR_BASE}" ]]; then
    echo "ERROR: ZEPHYR_BASE must point to the official Zephyr Git worktree." >&2
    exit 1
  fi
  local dirty
  dirty="$(git -C "${ZEPHYR_BASE}" status --porcelain --untracked-files=all)"
  if [[ -n "${dirty}" ]]; then
    echo "ERROR: Zephyr third-party source tree has local changes:" >&2
    printf '%s\n' "${dirty}" >&2
    exit 1
  fi
}

cleanup() {
  aicp_cleanup_process_tree "${qemu_pid:-}"
}
trap cleanup EXIT INT TERM

check_clean_zephyr

if ! command -v "${qemu_bin}" >/dev/null 2>&1; then
  echo "ERROR: QEMU executable was not found: ${qemu_bin}" >&2
  exit 1
fi
if [[ ! -f "${zephyr_elf}" ]]; then
  echo "ERROR: Zephyr ELF was not found: ${zephyr_elf}" >&2
  exit 1
fi
if ! [[ "${iterations}" =~ ^[1-9][0-9]*$ && "${boot_timeout_s}" =~ ^[1-9][0-9]*$ ]]; then
  echo "ERROR: iterations and boot timeout must be positive integers." >&2
  exit 2
fi

mkdir -p "${result_dir}"
make -C "${demo_dir}" build/aicp_client

echo "[ai-rtos] Starting stock Zephyr guest under QEMU"
"${qemu_bin}" \
  -global virtio-mmio.force-legacy=false \
  -cpu cortex-a53 \
  -nographic \
  -machine virt,secure=on,gic-version=3 \
  -netdev "user,id=u,hostfwd=tcp:127.0.0.1:${host_port}-10.0.2.15:8800" \
  -device e1000,netdev=u,mac=52:54:00:12:34:56 \
  -monitor none \
  -kernel "${zephyr_elf}" \
  >"${guest_log}" 2>&1 &
qemu_pid=$!

deadline=$((SECONDS + boot_timeout_s))
while (( SECONDS < deadline )); do
  if grep -q "AICP Zephyr RTOS server listening" "${guest_log}"; then
    break
  fi
  if ! kill -0 "${qemu_pid}" 2>/dev/null; then
    echo "[ai-rtos] FAIL: QEMU exited before the server became ready." >&2
    tail -n 80 "${guest_log}" >&2 || true
    exit 1
  fi
  sleep 0.2
done

if ! grep -q "AICP Zephyr RTOS server listening" "${guest_log}"; then
  echo "[ai-rtos] FAIL: Zephyr AICP server did not become ready." >&2
  tail -n 80 "${guest_log}" >&2 || true
  exit 1
fi

"${client}" 127.0.0.1 "${host_port}" "${iterations}" "${csv_file}" ai \
  2>&1 | tee "${client_log}"

grep -q "AICP client complete: ok=${iterations} failed=0" "${client_log}"
aicp_logs_match_regex "$(aicp_protocol_event_regex hello)" "${guest_log}"
aicp_logs_match_regex "$(aicp_protocol_event_regex control)" "${guest_log}"

check_clean_zephyr

echo "[ai-rtos] PASS: stock Zephyr source remained clean"
echo "[ai-rtos] Guest log: ${guest_log}"
echo "[ai-rtos] Client log: ${client_log}"
echo "[ai-rtos] CSV: ${csv_file}"
