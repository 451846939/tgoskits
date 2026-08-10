#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
#
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  scripts/ai-rtos/run_freertos_baseline.sh [timeout_seconds]

Runs the bundled QEMU/AArch64 FreeRTOS benchmark guest under AxVisor and
extracts benchmark min/avg/max timing tables into a reproducible result
directory.
EOF
}

if [[ $# -gt 1 || "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage >&2
  exit 2
fi

timeout_s="${1:-60}"
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
result_dir="${repo_root}/tmp/ai-rtos/results/freertos-baseline-$(date +%Y%m%d-%H%M%S)"
log_dir="${repo_root}/tmp/ai-rtos/logs"
mkdir -p "${result_dir}" "${log_dir}"

before="$(mktemp)"
after="$(mktemp)"
find "${log_dir}" -maxdepth 1 -type f -name 'freertos-boot-*.log' -print 2>/dev/null | sort > "${before}" || true

"${repo_root}/scripts/ai-rtos/run_rtos_boot_smoke.sh" freertos "${timeout_s}"

find "${log_dir}" -maxdepth 1 -type f -name 'freertos-boot-*.log' -print 2>/dev/null | sort > "${after}" || true
log_file="$(comm -13 "${before}" "${after}" | tail -n 1)"
rm -f "${before}" "${after}"
if [[ -z "${log_file}" ]]; then
  echo "[ai-rtos] FAIL: cannot locate FreeRTOS boot log" >&2
  exit 1
fi

cp "${log_file}" "${result_dir}/freertos.log"
"${repo_root}/scripts/ai-rtos/extract_freertos_benchmark.py" \
  "${log_file}" \
  --summary "${result_dir}/freertos.summary.txt"

echo "[ai-rtos] FreeRTOS baseline summary:"
cat "${result_dir}/freertos.summary.txt"
echo "[ai-rtos] PASS: FreeRTOS baseline artifacts are in ${result_dir}"
