#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
#
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

iterations="${1:-10000}"
boot_timeout_s="${2:-4200}"
stress_procs="${3:-2}"
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
result_dir="${repo_root}/tmp/ai-rtos/results/long-stability-$(date +%Y%m%d-%H%M%S)"
runner_output="${result_dir}/runner-output.log"
source "${repo_root}/scripts/ai-rtos/lib/run_artifacts.sh"

if ! [[ "${iterations}" =~ ^[1-9][0-9]*$ ]]; then
  echo "ERROR: iterations must be a positive integer" >&2
  exit 2
fi
if ! [[ "${stress_procs}" =~ ^[0-9]+$ ]] || (( stress_procs > 16 )); then
  echo "ERROR: stress_procs must be an integer in [0, 16]" >&2
  exit 2
fi

mkdir -p "${result_dir}"

start_epoch="$(date +%s)"
AICP_STRESS_PROCS="${stress_procs}" \
  "${repo_root}/scripts/ai-rtos/runners/run_axvisor_dual_guest_aicp.sh" \
  "${iterations}" ai "${boot_timeout_s}" | tee "${runner_output}"
end_epoch="$(date +%s)"

aicp_archive_dual_guest_logs "${runner_output}" "${result_dir}"
python3 "${repo_root}/scripts/ai-rtos/analysis/extract_aicp_log.py" \
  "${result_dir}/run.log" \
  --csv "${result_dir}/samples.csv" \
  --summary "${result_dir}/summary.txt" \
  --expected "${iterations}"

duration_s=$((end_epoch - start_epoch))
printf 'wall_duration_s=%s\n' "${duration_s}" >> "${result_dir}/summary.txt"
printf 'requested_iterations=%s\n' "${iterations}" >> "${result_dir}/summary.txt"
printf 'stress_procs=%s\n' "${stress_procs}" >> "${result_dir}/summary.txt"

grep -qx "samples=${iterations}" "${result_dir}/summary.txt"
grep -qx 'success_rate=1.000000' "${result_dir}/summary.txt"
grep -qx 'done_failed=0' "${result_dir}/summary.txt"
grep -qx 'reliability_timeouts=0' "${result_dir}/summary.txt"
grep -qx 'reliability_error_notifications=0' "${result_dir}/summary.txt"
grep -Eq '^rtos_request_timing_samples=[1-9][0-9]*$' "${result_dir}/summary.txt"
grep -Eq '^rtos_periodic_samples=[1-9][0-9]*$' "${result_dir}/summary.txt"
grep -Eq '^rt_trace_lines=[1-9][0-9]*$' "${result_dir}/summary.txt"

cat "${result_dir}/summary.txt"
echo "[ai-rtos] PASS: long-stability artifacts are in ${result_dir}"
