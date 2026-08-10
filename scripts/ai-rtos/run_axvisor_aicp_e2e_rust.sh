#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
#
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  scripts/ai-rtos/run_axvisor_aicp_e2e_rust.sh [iterations] [ai|fixed] [boot_timeout_seconds]

Boots AxVisor with the ArceOS AICP RTOS guest, then runs the Rust AICP client
through QEMU host forwarding:

  host/Rust client 127.0.0.1:18800 -> QEMU user-net -> RTOS guest 10.0.2.15:8800
EOF
}

if [[ $# -gt 3 || "${1:-}" == "-h" ]]; then
  usage >&2
  exit 2
fi

iterations="${1:-50}"
mode="${2:-ai}"
boot_timeout_s="${3:-90}"

if [[ "${mode}" != "ai" && "${mode}" != "fixed" ]]; then
  usage >&2
  exit 2
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
source "${repo_root}/scripts/ai-rtos/lib/markers.sh"
source "${repo_root}/scripts/ai-rtos/lib/process.sh"
demo_dir="${repo_root}/apps/ai-rtos-demo"
log_dir="${repo_root}/tmp/ai-rtos/logs"
mkdir -p "${log_dir}" "${demo_dir}/build"

stamp="$(date +%Y%m%d-%H%M%S)"
log_file="${log_dir}/axvisor-aicp-e2e-rust-${stamp}.log"
client_log="${log_dir}/aicp-rust-client-${stamp}.log"
csv_file="${demo_dir}/build/axvisor_aicp_rust_${mode}_${stamp}.csv"
summary_file="${demo_dir}/build/axvisor_aicp_rust_${mode}_${stamp}.summary.txt"

cleanup() {
  aicp_cleanup_process_tree "${qemu_pid:-}"
}
trap cleanup EXIT

wait_for_marker() {
  aicp_wait_for_marker "$1" "$((SECONDS + boot_timeout_s))" \
    "${qemu_pid}" "${log_file}" 120
}

echo "[ai-rtos] Preparing RTOS guest and Rust AICP client..."
"${repo_root}/scripts/ai-rtos/setup_qemu_rtos.sh" arceos-aicp
make -C "${demo_dir}" rust-client

echo "[ai-rtos] Booting AxVisor; log: ${log_file}"
(
  cd "${repo_root}"
  cargo xtask axvisor qemu \
    --config os/axvisor/configs/board/qemu-aarch64.toml \
    --qemu-config os/axvisor/configs/qemu/qemu-aarch64-aicp-net.toml \
    --vmconfigs tmp/ai-rtos/arceos-aicp-qemu.generated.toml
) >"${log_file}" 2>&1 &
qemu_pid=$!

wait_for_marker "registered virtio network device"
wait_for_marker "DHCP acquired address"
aicp_wait_for_arceos_ready \
  "$((SECONDS + boot_timeout_s))" "${qemu_pid}" "${log_file}" 120

echo "[ai-rtos] Running Rust AICP client iterations=${iterations} mode=${mode}"
"${demo_dir}/build/aicp_rust_client" 127.0.0.1 18800 "${iterations}" "${csv_file}" "${mode}" \
  2>&1 | tee "${client_log}"
"${repo_root}/scripts/ai-rtos/summarize_latency.py" "${csv_file}" "${iterations}" | tee "${summary_file}"

if ! grep -q "AICP_RUST_DONE ok=.*failed=0" "${client_log}"; then
  echo "[ai-rtos] FAIL: Rust client reported failures" >&2
  tail -n 120 "${client_log}" >&2 || true
  exit 1
fi
if ! grep -q "CONTROL seq=" "${log_file}"; then
  echo "[ai-rtos] FAIL: RTOS guest did not log CONTROL messages" >&2
  tail -n 120 "${log_file}" >&2 || true
  exit 1
fi

echo "[ai-rtos] PASS: AxVisor Rust AICP e2e complete"
echo "[ai-rtos] log: ${log_file}"
echo "[ai-rtos] client_log: ${client_log}"
echo "[ai-rtos] csv: ${csv_file}"
echo "[ai-rtos] summary: ${summary_file}"
