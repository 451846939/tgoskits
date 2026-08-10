#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
demo_dir="${repo_root}/apps/ai-rtos-demo"
port="${AICP_DELAYED_SERVER_PORT:-18809}"
iterations="${AICP_DELAYED_SERVER_ITERATIONS:-5}"
delay_s="${AICP_DELAYED_SERVER_DELAY_S:-0.35}"
result_dir="${AICP_DELAYED_SERVER_RESULT_DIR:-${repo_root}/tmp/ai-rtos/results/delayed-server-reconnect}"

mkdir -p "${result_dir}"
make -C "${demo_dir}" all

client_log="${result_dir}/client.log"
server_log="${result_dir}/server.log"
csv="${result_dir}/latency.csv"
server_pid=""

cleanup() {
    if [[ -n "${server_pid}" ]]; then
        kill "${server_pid}" 2>/dev/null || true
        wait "${server_pid}" 2>/dev/null || true
    fi
}
trap cleanup EXIT

"${demo_dir}/build/aicp_client" 127.0.0.1 "${port}" "${iterations}" "${csv}" ai \
    >"${client_log}" 2>&1 &
client_pid=$!

sleep "${delay_s}"
"${demo_dir}/build/aicp_server" "${port}" >"${server_log}" 2>&1 &
server_pid=$!

if ! wait "${client_pid}"; then
    cat "${client_log}" >&2
    echo "AICP_DELAYED_SERVER_SUMMARY ok=0 expected=${iterations}" >&2
    exit 1
fi

ok="$(sed -n 's/.*AICP client complete:.*ok=\([0-9][0-9]*\).*/\1/p' "${client_log}" | tail -n 1)"
failed="$(sed -n 's/.*AICP client complete:.*failed=\([0-9][0-9]*\).*/\1/p' "${client_log}" | tail -n 1)"

cat "${client_log}"
cat "${server_log}"
echo "AICP_DELAYED_SERVER_SUMMARY ok=${ok:-0} failed=${failed:-unknown} expected=${iterations} delay_s=${delay_s}"

if [[ "${ok:-0}" != "${iterations}" || "${failed:-1}" != "0" ]]; then
    exit 1
fi
