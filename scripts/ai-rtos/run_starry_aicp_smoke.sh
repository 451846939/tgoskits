#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
#
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  scripts/ai-rtos/run_starry_aicp_smoke.sh [iterations] [ai|fixed]

Runs the StarryOS AICP client as a standalone QEMU app.  The RTOS side is
represented by the host reference server on 10.0.2.2:8800 through QEMU user-net.
EOF
}

if [[ $# -gt 2 || "${1:-}" == "-h" ]]; then
  usage >&2
  exit 2
fi

iterations="${1:-20}"
mode="${2:-ai}"
if [[ "${mode}" != "ai" && "${mode}" != "fixed" ]]; then
  usage >&2
  exit 2
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
source "${repo_root}/scripts/ai-rtos/lib/host_tools.sh"
demo_dir="${repo_root}/apps/ai-rtos-demo"
log_dir="${repo_root}/tmp/ai-rtos/logs"
mkdir -p "${log_dir}"
stamp="$(date +%Y%m%d-%H%M%S)"
server_log="${log_dir}/starry-aicp-host-server-${stamp}.log"
qemu_log="${log_dir}/starry-aicp-smoke-${stamp}.log"
port="${AICP_STARRY_SERVER_PORT:-8800}"

cleanup() {
  if [[ -n "${server_pid:-}" ]]; then
    kill "${server_pid}" 2>/dev/null || true
    wait "${server_pid}" 2>/dev/null || true
  fi
}
trap cleanup EXIT

debugfs_bin="$(aicp_resolve_tool DEBUGFS debugfs || true)"
if [[ -z "${debugfs_bin}" ]]; then
  cat >&2 <<'EOF'
[ai-rtos] ERROR: debugfs is required by `cargo xtask starry app qemu` to inject
the AICP client into the StarryOS ext rootfs. Install e2fsprogs, or rerun with:

  DEBUGFS=/path/to/debugfs scripts/ai-rtos/run_starry_aicp_smoke.sh ...
EOF
  exit 1
fi
export DEBUGFS="${debugfs_bin}"

echo "[ai-rtos] Building host AICP reference server..."
make -C "${demo_dir}" "${demo_dir}/build/aicp_server"

echo "[ai-rtos] Starting host RTOS reference server on 0.0.0.0:${port}; log: ${server_log}"
"${demo_dir}/build/aicp_server" "${port}" >"${server_log}" 2>&1 &
server_pid=$!
sleep 0.3
if ! kill -0 "${server_pid}" 2>/dev/null; then
  echo "[ai-rtos] host AICP server exited early" >&2
  cat "${server_log}" >&2 || true
  exit 1
fi

echo "[ai-rtos] Booting StarryOS standalone AICP app; log: ${qemu_log}"
(
  cd "${repo_root}"
  AICP_STARRY_ITERATIONS="${iterations}" \
  AICP_STARRY_MODE="${mode}" \
  AICP_STARRY_SERVER="10.0.2.2" \
  AICP_STARRY_SERVER_PORT="${port}" \
  AICP_STARRY_CLIENT="10.0.2.15" \
  AICP_STARRY_NET_PREFIX="10.0.2.0" \
  AICP_STARRY_STATIC_ARP="0" \
    cargo xtask starry app qemu -t aicp-control --arch aarch64
) >"${qemu_log}" 2>&1

if ! grep -q "AICP_STARRY_DONE ok=.*failed=0" "${qemu_log}"; then
  echo "[ai-rtos] FAIL: StarryOS AICP app did not report success" >&2
  tail -n 160 "${qemu_log}" >&2 || true
  exit 1
fi

grep "AICP_STARRY_DONE" "${qemu_log}" | tail -n 1
echo "[ai-rtos] PASS: StarryOS standalone AICP smoke complete"
echo "[ai-rtos] qemu log: ${qemu_log}"
echo "[ai-rtos] server log: ${server_log}"
