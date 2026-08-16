#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
source "${repo_root}/scripts/ai-rtos/lib/run_artifacts.sh"

test_dir="$(mktemp -d "${TMPDIR:-/tmp}/aicp-run-artifacts.XXXXXX")"
trap 'rm -rf "${test_dir}"' EXIT

source_dir="${test_dir}/source"
result_dir="${test_dir}/result"
mkdir -p "${source_dir}" "${result_dir}"

qemu_log="${source_dir}/axvisor-dual-guest-aicp-c-20260731-120000.log"
linux_log="${source_dir}/axvisor-dual-guest-aicp-c-linux-console-20260731-120000.log"
runner_output="${test_dir}/runner-output.log"

printf '%s\n' \
  'rt-trace: vm=2 event=irq_inject duration_ns=1234' \
  'AICP_RTOS_REQUEST_TIMING seq=1 service_ns=1000 request_interval_ns=0 request_interval_deviation_ns=0' \
  'AICP_RTOS_PERIODIC_DONE samples=128 period_ns=20000000 wake_lateness_avg_ns=1000 wake_lateness_p99_ns=2000 wake_lateness_max_ns=3000 interval_abs_jitter_avg_ns=900 interval_abs_jitter_p99_ns=1800 interval_abs_jitter_max_ns=2600 missed_deadlines=0' > "${qemu_log}"
printf '%s\n' \
  'AICP_LINUX_STATUS seq=1 rtt_ns=2000' \
  'AICP_LINUX_DONE ok=1 failed=0' > "${linux_log}"
printf '%s\n' \
  "[ai-rtos] AxVisor log: ${qemu_log}" \
  "[ai-rtos] Linux console log: ${linux_log}" \
  "log=${qemu_log}" \
  "linux_console_log=${linux_log}" > "${runner_output}"

aicp_archive_dual_guest_logs "${runner_output}" "${result_dir}"

cmp -s "${qemu_log}" "${result_dir}/qemu.log"
cmp -s "${linux_log}" "${result_dir}/linux-console.log"
grep -q '^rt-trace:' "${result_dir}/run.log"
grep -q '^AICP_LINUX_DONE ok=1 failed=0$' "${result_dir}/run.log"

combined_result="${test_dir}/combined-result"
combined_output="${test_dir}/combined-runner-output.log"
printf 'log=%s\n' "${qemu_log}" > "${combined_output}"
aicp_archive_dual_guest_logs "${combined_output}" "${combined_result}"
cmp -s "${qemu_log}" "${combined_result}/qemu.log"
cmp -s "${qemu_log}" "${combined_result}/run.log"
if [[ -e "${combined_result}/linux-console.log" ]]; then
  echo "FAIL: 合并日志不应生成独立 Linux console 归档" >&2
  exit 1
fi

invalid_output="${test_dir}/invalid-runner-output.log"
printf 'summary=%s\n' "${test_dir}/summary.txt" > "${invalid_output}"
if aicp_archive_dual_guest_logs "${invalid_output}" "${test_dir}/invalid-result" 2>/dev/null; then
  echo "FAIL: 缺少主日志路径时归档仍然成功" >&2
  exit 1
fi

echo "PASS: split and multiplexed dual-guest logs are archived from explicit runner paths"
