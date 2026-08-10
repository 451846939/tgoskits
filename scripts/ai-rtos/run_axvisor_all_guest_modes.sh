#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
#
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  scripts/ai-rtos/run_axvisor_all_guest_modes.sh [iterations] [boot_timeout_seconds]

Runs the full AxVisor AI/RTOS guest matrix:
  1. Linux   + RTOS, fixed baseline
  2. Linux   + RTOS, AI adaptive control
  3. StarryOS+ RTOS, fixed baseline
  4. StarryOS+ RTOS, AI adaptive control

The Linux path is the required scoring baseline. The StarryOS path is additive
and does not replace the Linux runner.
EOF
}

if [[ $# -gt 2 || "${1:-}" == "-h" ]]; then
  usage >&2
  exit 2
fi

iterations="${1:-20}"
boot_timeout_s="${2:-240}"
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
result_dir="${repo_root}/tmp/ai-rtos/results/all-modes-$(date +%Y%m%d-%H%M%S)"
mkdir -p "${result_dir}"

run_case() {
  local guest="$1"
  local mode="$2"
  local marker="$3"
  local log_pattern="$4"
  local runner="$5"
  local before after log summary

  before="$(mktemp)"
  after="$(mktemp)"
  find "${repo_root}/tmp/ai-rtos/logs" -maxdepth 1 -type f -name "${log_pattern}" -print 2>/dev/null | sort > "${before}" || true

  echo "[ai-rtos] RUN ${guest} mode=${mode} iterations=${iterations}"
  "${runner}" "${iterations}" "${mode}" "${boot_timeout_s}"

  find "${repo_root}/tmp/ai-rtos/logs" -maxdepth 1 -type f -name "${log_pattern}" -print 2>/dev/null | sort > "${after}" || true
  log="$(comm -13 "${before}" "${after}" | tail -n 1)"
  rm -f "${before}" "${after}"

  if [[ -z "${log}" ]]; then
    echo "[ai-rtos] FAIL: cannot locate ${guest}/${mode} log" >&2
    exit 1
  fi
  if ! grep -q "${marker} ok=.*failed=0" "${log}"; then
    echo "[ai-rtos] FAIL: ${guest}/${mode} did not report a clean completion" >&2
    tail -n 160 "${log}" >&2 || true
    exit 1
  fi

  cp "${log}" "${result_dir}/${guest}-${mode}.log"
  summary="$(grep "${marker}" "${log}" | tail -n 1)"
  printf '%s mode=%s log=%s %s\n' "${guest}" "${mode}" "${log}" "${summary}" | tee -a "${result_dir}/summary.txt"
}

linux_runner="${repo_root}/scripts/ai-rtos/run_axvisor_dual_guest_aicp.sh"
starry_runner="${repo_root}/scripts/ai-rtos/run_axvisor_starry_rtos_aicp.sh"

export AICP_QEMU_NET_BACKEND="${AICP_QEMU_NET_BACKEND:-hub}"
export AICP_STARRY_NATIVE="${AICP_STARRY_NATIVE:-1}"
export AICP_STARRY_UDP_RETRIES="${AICP_STARRY_UDP_RETRIES:-8}"

run_case "linux-rtos" "fixed" "AICP_LINUX_DONE" "axvisor-dual-guest-aicp-*.log" "${linux_runner}"
run_case "linux-rtos" "ai" "AICP_LINUX_DONE" "axvisor-dual-guest-aicp-*.log" "${linux_runner}"
run_case "starry-rtos" "fixed" "AICP_STARRY_DONE" "axvisor-starry-rtos-aicp-*.log" "${starry_runner}"
run_case "starry-rtos" "ai" "AICP_STARRY_DONE" "axvisor-starry-rtos-aicp-*.log" "${starry_runner}"

echo "[ai-rtos] PASS: all AxVisor guest modes completed"
echo "[ai-rtos] results: ${result_dir}"
