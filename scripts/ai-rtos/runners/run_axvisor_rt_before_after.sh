#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
#
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  scripts/ai-rtos/runners/run_axvisor_rt_before_after.sh [iterations] [boot_timeout_seconds] [stress_procs] [rounds]

Runs the same dual-guest AICP workload with two AxVisor variants:
  optimized - board config selected by AICP_OPTIMIZED_BOARD_CONFIG
  baseline  - board config selected by AICP_BASELINE_BOARD_CONFIG

The default baseline falls back to the old shared vCPU wait queue and broadcast
wake path. Either config can be overridden for a single-variable experiment.
With multiple rounds, execution order alternates to reduce order bias. Every
round keeps raw artifacts and the final report summarizes medians, ranges, and
worst values.

Optional environment variables:
  AICP_OPTIMIZED_BOARD_CONFIG - optimized board config override
  AICP_BASELINE_BOARD_CONFIG  - baseline board config override

The optimized default intentionally does not enable rt-preempt. The optional
qemu-aarch64-rt-preempt.toml config is retained for shared-pCPU experiments,
but adds unnecessary scheduler IPIs in the default one-vCPU-per-pCPU layout.
EOF
}

if [[ $# -gt 4 || "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage >&2
  exit 2
fi

iterations="${1:-300}"
boot_timeout_s="${2:-360}"
stress_procs="${3:-2}"
rounds="${4:-1}"
optimized_board_config="${AICP_OPTIMIZED_BOARD_CONFIG:-os/axvisor/configs/board/qemu-aarch64-rt.toml}"
baseline_board_config="${AICP_BASELINE_BOARD_CONFIG:-os/axvisor/configs/board/qemu-aarch64-rt-shared-wait-baseline.toml}"
if ! [[ "${rounds}" =~ ^[0-9]+$ ]] || (( rounds == 0 )); then
  echo "ERROR: rounds must be a positive integer" >&2
  exit 2
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
result_dir="${repo_root}/tmp/ai-rtos/results/rt-before-after-$(date +%Y%m%d-%H%M%S)"
mkdir -p "${result_dir}"

run_variant() {
  local variant="$1"
  local board_config="$2"
  local destination_root="$3"
  local before after latest matrix_dir
  local matrix_status=0
  before="$(mktemp)"
  after="$(mktemp)"
  find "${repo_root}/tmp/ai-rtos/results" -maxdepth 1 -type d -name 'realtime-*' -print 2>/dev/null | sort > "${before}" || true

  echo "[ai-rtos] Running ${variant} realtime matrix"
  if AICP_AXVISOR_BOARD_CONFIG="${board_config}" \
    "${repo_root}/scripts/ai-rtos/runners/run_axvisor_realtime_matrix.sh" \
      "${iterations}" "${boot_timeout_s}" "${stress_procs}"; then
    :
  else
    matrix_status=$?
  fi

  find "${repo_root}/tmp/ai-rtos/results" -maxdepth 1 -type d -name 'realtime-*' -print 2>/dev/null | sort > "${after}" || true
  latest="$(comm -13 "${before}" "${after}" | tail -n 1)"
  rm -f "${before}" "${after}"
  if [[ -z "${latest}" ]]; then
    echo "[ai-rtos] FAIL: cannot locate ${variant} realtime result directory" >&2
    matrix_dir="${destination_root}/${variant}"
    mkdir -p "${matrix_dir}"
    {
      echo "status=FAIL"
      echo "matrix_status=${matrix_status}"
      echo "reason=realtime result directory not found"
    } > "${matrix_dir}/variant.status.txt"
    return 1
  fi

  matrix_dir="${destination_root}/${variant}"
  mkdir -p "${matrix_dir}"
  cp -R "${latest}/." "${matrix_dir}/"
  if (( matrix_status != 0 )); then
    {
      echo "status=FAIL"
      echo "matrix_status=${matrix_status}"
      echo "reason=one or more realtime cases failed"
    } > "${matrix_dir}/variant.status.txt"
    return 1
  fi
  {
    echo "status=PASS"
    echo "matrix_status=0"
    echo "board_config=${board_config}"
  } > "${matrix_dir}/variant.status.txt"
}

round_dirs=()
failed_variants=0
for ((round = 1; round <= rounds; round++)); do
  if (( rounds == 1 )); then
    round_dir="${result_dir}"
  else
    printf -v round_name 'round-%02d' "${round}"
    round_dir="${result_dir}/${round_name}"
    mkdir -p "${round_dir}"
  fi
  round_dirs+=("${round_dir}")

  if (( round % 2 == 1 )); then
    variants=(baseline optimized)
  else
    variants=(optimized baseline)
  fi
  for variant in "${variants[@]}"; do
    if [[ "${variant}" == "baseline" ]]; then
      board_config="${baseline_board_config}"
    else
      board_config="${optimized_board_config}"
    fi
    if ! run_variant "${variant}" "${board_config}" "${round_dir}"; then
      failed_variants=$((failed_variants + 1))
    fi
  done

  python3 "${repo_root}/scripts/ai-rtos/analysis/summarize_rt_before_after.py" \
    "${round_dir}/baseline" \
    "${round_dir}/optimized" \
    --summary "${round_dir}/before_after.summary.txt"
  cat "${round_dir}/before_after.summary.txt"
done

if (( rounds > 1 )); then
  python3 "${repo_root}/scripts/ai-rtos/analysis/summarize_rt_multirun.py" \
    "${round_dirs[@]}" \
    --summary "${result_dir}/multirun.summary.txt"
  cat "${result_dir}/multirun.summary.txt"
fi

if (( failed_variants != 0 )); then
  echo "[ai-rtos] FAIL: ${failed_variants} realtime variant run(s) failed; artifacts are in ${result_dir}" >&2
  exit 1
fi

echo "[ai-rtos] PASS: before/after realtime artifacts are in ${result_dir}"
