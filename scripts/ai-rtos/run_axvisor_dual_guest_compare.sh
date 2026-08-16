#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
#
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  scripts/ai-rtos/run_axvisor_dual_guest_compare.sh [iterations] [boot_timeout_seconds]

Runs the QEMU/AxVisor dual-guest AICP closed loop twice:
  fixed  - fixed PID-like control parameters
  ai     - neural-network adaptive parameters

The script stores raw QEMU logs, extracted CSV files, latency summaries, and a
fixed-vs-AI comparison under tmp/ai-rtos/results. By default it uses AxVisor's
internal layer-2 switch; set AICP_TRANSPORT=usernet for the hostfwd fallback
path.
EOF
}

if [[ $# -gt 2 || "${1:-}" == "-h" ]]; then
  usage >&2
  exit 2
fi

iterations="${1:-30}"
boot_timeout_s="${2:-180}"
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
source "${repo_root}/scripts/ai-rtos/lib/process.sh"
result_dir="${repo_root}/tmp/ai-rtos/results/compare-$(date +%Y%m%d-%H%M%S)"
rootfs_image="${repo_root}/tmp/axbuild/rootfs/rootfs-aarch64-alpine.img/rootfs-aarch64-alpine.img"
mkdir -p "${result_dir}"

run_mode() {
  local mode="$1"
  local before after log combined_log csv summary
  local client_impl="${AICP_CLIENT_IMPL:-c}"
  local log_prefix="axvisor-dual-guest-aicp-${client_impl}"
  local log_glob="${log_prefix}-[0-9]*.log"
  local runner="${repo_root}/scripts/ai-rtos/run_axvisor_dual_guest_aicp.sh"

  if [[ "${AICP_TRANSPORT:-hub}" == "usernet" ]]; then
    log_glob='axvisor-dual-guest-aicp-usernet-*.log'
    runner="${repo_root}/scripts/ai-rtos/run_axvisor_dual_guest_aicp_usernet.sh"
  elif [[ "${AICP_TRANSPORT:-hub}" != "hub" ]]; then
    echo "ERROR: AICP_TRANSPORT must be hub or usernet" >&2
    exit 2
  fi

  before="$(mktemp)"
  after="$(mktemp)"
  find "${repo_root}/tmp/ai-rtos/logs" -maxdepth 1 -type f -name "${log_glob}" -print 2>/dev/null | sort > "${before}" || true

  if [[ "${AICP_TRANSPORT:-hub}" == "usernet" ]]; then
    AICP_HOST_PORT="${AICP_HOST_PORT:-18800}" "${runner}" "${iterations}" "${mode}" "${boot_timeout_s}"
  else
    "${runner}" "${iterations}" "${mode}" "${boot_timeout_s}"
  fi
  aicp_wait_for_qemu_image_release "${rootfs_image}" 20

  find "${repo_root}/tmp/ai-rtos/logs" -maxdepth 1 -type f -name "${log_glob}" -print 2>/dev/null | sort > "${after}" || true
  log="$(comm -13 "${before}" "${after}" | tail -n 1)"
  rm -f "${before}" "${after}"
  if [[ -z "${log}" ]]; then
    echo "[ai-rtos] FAIL: cannot locate ${mode} QEMU log" >&2
    exit 1
  fi

  cp "${log}" "${result_dir}/${mode}.axvisor.log"
  combined_log="${result_dir}/${mode}.log"
  if [[ "${AICP_TRANSPORT:-hub}" == "hub" ]]; then
    # AxVisor's console mux writes host and both guest consoles into one log.
    cp "${log}" "${combined_log}"
  else
    cp "${log}" "${combined_log}"
  fi
  csv="${result_dir}/${mode}.csv"
  summary="${result_dir}/${mode}.summary.txt"
  "${repo_root}/scripts/ai-rtos/extract_aicp_log.py" \
    "${combined_log}" --csv "${csv}" --summary "${summary}" --expected "${iterations}"
  echo "[ai-rtos] ${mode} summary:"
  cat "${summary}"
}

run_mode fixed
run_mode ai

"${repo_root}/scripts/ai-rtos/compare_control.py" \
  "${result_dir}/fixed.csv" \
  "${result_dir}/ai.csv" \
  "${iterations}" | tee "${result_dir}/compare.summary.txt"

echo "[ai-rtos] PASS: comparison artifacts are in ${result_dir}"
