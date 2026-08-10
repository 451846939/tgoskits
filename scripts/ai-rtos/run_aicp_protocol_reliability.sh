#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Run host-side AICP protocol reliability checks and collect reproducible logs.
#
# Usage:
#   scripts/ai-rtos/run_aicp_protocol_reliability.sh [smoke_iterations]

set -euo pipefail

repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
iterations="${1:-50}"
stamp="$(date +%Y%m%d-%H%M%S)"
result_dir="${repo_root}/tmp/ai-rtos/results/protocol-${stamp}"
log_dir="${repo_root}/tmp/ai-rtos/logs"
demo_dir="${repo_root}/apps/ai-rtos-demo"

mkdir -p "${result_dir}" "${log_dir}"

protocol_log="${result_dir}/protocol-test.log"
smoke_log="${result_dir}/host-smoke.log"
delayed_server_log="${result_dir}/delayed-server-reconnect.log"
summary="${result_dir}/summary.txt"

echo "[ai-rtos] running AICP protocol negative/edge-case tests"
make -C "${demo_dir}" protocol-test 2>&1 | tee "${protocol_log}"

echo "[ai-rtos] running host TCP smoke iterations=${iterations}"
AICP_ITERATIONS="${iterations}" make -C "${demo_dir}" smoke 2>&1 | tee "${smoke_log}"

echo "[ai-rtos] running delayed-server reconnect regression"
AICP_DELAYED_SERVER_RESULT_DIR="${result_dir}/delayed-server" \
  "${repo_root}/scripts/ai-rtos/test_aicp_delayed_server_reconnect.sh" 2>&1 | tee "${delayed_server_log}"

passed="$(awk -F'[ =]' '/AICP_PROTOCOL_SUMMARY/ {for (i=1;i<=NF;i++) if ($i=="passed") print $(i+1)}' "${protocol_log}" | tail -n 1)"
failed="$(awk -F'[ =]' '/AICP_PROTOCOL_SUMMARY/ {for (i=1;i<=NF;i++) if ($i=="failed") print $(i+1)}' "${protocol_log}" | tail -n 1)"
smoke_ok="$(sed -n 's/.*AICP client complete:.*ok=\([0-9][0-9]*\).*/\1/p' "${smoke_log}" | tail -n 1)"
smoke_failed="$(sed -n 's/.*AICP client complete:.*failed=\([0-9][0-9]*\).*/\1/p' "${smoke_log}" | tail -n 1)"
avg_rtt="$(awk -F= '/^rtt_ns_avg=/ {print $2}' "${smoke_log}" | tail -n 1)"
max_rtt="$(awk -F= '/^rtt_ns_max=/ {print $2}' "${smoke_log}" | tail -n 1)"
delayed_ok="$(sed -n 's/.*AICP_DELAYED_SERVER_SUMMARY.*ok=\([0-9][0-9]*\).*/\1/p' "${delayed_server_log}" | tail -n 1)"
delayed_failed="$(sed -n 's/.*AICP_DELAYED_SERVER_SUMMARY.*failed=\([0-9][0-9]*\).*/\1/p' "${delayed_server_log}" | tail -n 1)"

{
  echo "protocol_cases_passed=${passed:-0}"
  echo "protocol_cases_failed=${failed:-unknown}"
  echo "host_smoke_iterations=${iterations}"
  echo "host_smoke_ok=${smoke_ok:-0}"
  echo "host_smoke_failed=${smoke_failed:-unknown}"
  echo "host_smoke_avg_rtt_ns=${avg_rtt:-unknown}"
  echo "host_smoke_max_rtt_ns=${max_rtt:-unknown}"
  echo "delayed_server_ok=${delayed_ok:-0}"
  echo "delayed_server_failed=${delayed_failed:-unknown}"
  echo "protocol_log=${protocol_log}"
  echo "host_smoke_log=${smoke_log}"
  echo "delayed_server_log=${delayed_server_log}"
} | tee "${summary}"

if [[ "${failed:-1}" != "0" || "${smoke_ok:-0}" != "${iterations}" || "${smoke_failed:-1}" != "0" || "${delayed_ok:-0}" != "5" || "${delayed_failed:-1}" != "0" ]]; then
  echo "[ai-rtos] FAIL: protocol reliability checks failed; artifacts are in ${result_dir}" >&2
  exit 1
fi

echo "[ai-rtos] PASS: protocol reliability artifacts are in ${result_dir}"
