#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
#
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  scripts/ai-rtos/run_axvisor_realtime_matrix.sh [iterations] [boot_timeout_seconds] [stress_procs]

Runs the verified AxVisor dual-guest AICP TCP/IP scenario twice:
  idle   - no synthetic Linux guest background load
  stress - Linux guest starts stress_procs busy worker processes

The extractor reports Linux request-response RTT, RTOS-side control service
time, request-arrival deviation, and independent 20 ms RTOS periodic wakeup
lateness/jitter from AICP_RTOS_REQUEST_TIMING and AICP_RTOS_PERIODIC_DONE lines.
By default the scenario uses AxVisor's internal layer-2 switch; set
AICP_TRANSPORT=usernet for the hostfwd fallback path.
EOF
}

if [[ $# -gt 3 || "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage >&2
  exit 2
fi

iterations="${1:-200}"
boot_timeout_s="${2:-240}"
stress_procs="${3:-2}"
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
source "${repo_root}/scripts/ai-rtos/lib/process.sh"
result_dir="${repo_root}/tmp/ai-rtos/results/realtime-$(date +%Y%m%d-%H%M%S)"
rootfs_image="${repo_root}/tmp/axbuild/rootfs/rootfs-aarch64-alpine.img/rootfs-aarch64-alpine.img"
mkdir -p "${result_dir}"

if ! [[ "${iterations}" =~ ^[0-9]+$ ]] || (( iterations == 0 )); then
  echo "ERROR: iterations must be a positive integer" >&2
  exit 2
fi
if ! [[ "${stress_procs}" =~ ^[0-9]+$ ]] || (( stress_procs > 16 )); then
  echo "ERROR: stress_procs must be an integer in [0, 16]" >&2
  exit 2
fi

run_case() {
  local name="$1"
  local load="$2"
  local before after log combined_log csv summary
  local runner_status=0
  local extraction_status=0
  local failure_reason=""
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

  echo "[ai-rtos] Running realtime case=${name} iterations=${iterations} stress_procs=${load}"
  if [[ "${AICP_TRANSPORT:-hub}" == "usernet" ]]; then
    if AICP_HOST_PORT="${AICP_HOST_PORT:-18800}" AICP_STRESS_PROCS="${load}" \
      "${runner}" "${iterations}" ai "${boot_timeout_s}"; then
      :
    else
      runner_status=$?
    fi
  else
    if AICP_STRESS_PROCS="${load}" "${runner}" "${iterations}" ai "${boot_timeout_s}"; then
      :
    else
      runner_status=$?
    fi
  fi
  if ! aicp_wait_for_qemu_image_release "${rootfs_image}" 20; then
    runner_status=1
    failure_reason="QEMU image was not released"
  fi

  find "${repo_root}/tmp/ai-rtos/logs" -maxdepth 1 -type f -name "${log_glob}" -print 2>/dev/null | sort > "${after}" || true
  log="$(comm -13 "${before}" "${after}" | tail -n 1)"
  rm -f "${before}" "${after}"
  if [[ -z "${log}" ]]; then
    echo "[ai-rtos] FAIL: cannot locate ${name} QEMU log" >&2
    failure_reason="QEMU log not found"
    extraction_status=1
  else
    cp "${log}" "${result_dir}/${name}.axvisor.log"
    combined_log="${result_dir}/${name}.log"
    if [[ "${AICP_TRANSPORT:-hub}" == "hub" ]]; then
      # AxVisor's console mux writes host and both guest consoles into one log.
      cp "${log}" "${combined_log}"
    else
      cp "${log}" "${combined_log}"
    fi
  fi

  if [[ -n "${combined_log:-}" && -f "${combined_log}" ]]; then
    csv="${result_dir}/${name}.csv"
    summary="${result_dir}/${name}.summary.txt"
    if "${repo_root}/scripts/ai-rtos/extract_aicp_log.py" \
      "${combined_log}" --csv "${csv}" --summary "${summary}" --expected "${iterations}"; then
      :
    else
      extraction_status=$?
      if [[ -z "${failure_reason}" ]]; then
        failure_reason="incomplete or failed AICP samples"
      fi
    fi
    if [[ -f "${summary}" ]]; then
      echo "[ai-rtos] ${name} summary:"
      cat "${summary}"
    fi
  fi

  if (( runner_status != 0 || extraction_status != 0 )); then
    {
      echo "status=FAIL"
      echo "runner_status=${runner_status}"
      echo "extraction_status=${extraction_status}"
      echo "reason=${failure_reason:-runner failed}"
    } > "${result_dir}/${name}.status.txt"
    echo "[ai-rtos] FAIL: realtime case=${name}: ${failure_reason:-runner failed}" >&2
    return 1
  fi

  {
    echo "status=PASS"
    echo "runner_status=0"
    echo "extraction_status=0"
  } > "${result_dir}/${name}.status.txt"
}

failed_cases=0
if ! run_case idle 0; then
  failed_cases=$((failed_cases + 1))
fi
if ! run_case stress "${stress_procs}"; then
  failed_cases=$((failed_cases + 1))
fi

{
  echo "iterations=${iterations}"
  echo "stress_procs=${stress_procs}"
  echo
  echo "[idle]"
  cat "${result_dir}/idle.status.txt"
  if [[ -f "${result_dir}/idle.summary.txt" ]]; then
    cat "${result_dir}/idle.summary.txt"
  fi
  echo
  echo "[stress]"
  cat "${result_dir}/stress.status.txt"
  if [[ -f "${result_dir}/stress.summary.txt" ]]; then
    cat "${result_dir}/stress.summary.txt"
  fi
} > "${result_dir}/realtime.summary.txt"

if (( failed_cases != 0 )); then
  echo "[ai-rtos] FAIL: ${failed_cases} realtime case(s) failed; artifacts are in ${result_dir}" >&2
  exit 1
fi

echo "[ai-rtos] PASS: realtime matrix artifacts are in ${result_dir}"
