#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
#
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
iterations="${1:-20}"
drop_every="${AICP_UDP_DROP_EVERY:-3}"
result_dir="${repo_root}/tmp/ai-rtos/results/starry-udp-faults-$(date +%Y%m%d-%H%M%S)"
log_dir="${repo_root}/tmp/ai-rtos/logs"

mkdir -p "${result_dir}" "${log_dir}"
before="$(mktemp)"
after="$(mktemp)"
find "${log_dir}" -maxdepth 1 -type f -name 'axvisor-starry-*-aicp-*.log' -print | sort > "${before}" || true

echo "[ai-rtos] Running StarryOS UDP fault injection iterations=${iterations} drop_every=${drop_every}"
AICP_UDP_DROP_EVERY="${drop_every}" AICP_STARRY_UDP_RETRIES=8 AICP_STARRY_UDP_REORDER_TEST=1 \
  "${repo_root}/scripts/ai-rtos/run_axvisor_starry_rtos_aicp.sh" "${iterations}" ai 240

find "${log_dir}" -maxdepth 1 -type f -name 'axvisor-starry-*-aicp-*.log' -print | sort > "${after}" || true
log_file="$(comm -13 "${before}" "${after}" | tail -n 1)"
rm -f "${before}" "${after}"

if [[ -z "${log_file}" ]]; then
  echo "ERROR: cannot locate StarryOS UDP fault log" >&2
  exit 1
fi
cp "${log_file}" "${result_dir}/qemu.log"

fault_drops="$(grep -c 'AICP UDP fault_drop' "${log_file}" || true)"
duplicates="$(grep -c 'AICP UDP duplicate' "${log_file}" || true)"
out_of_order="$(grep -c 'AICP UDP out_of_order' "${log_file}" || true)"
stale_rejected="$(grep -c 'udp stale_sequence accepted=0' "${log_file}" || true)"
timeouts="$(grep -Ec 'AICP_STARRY_.*TIMEOUT|AICP StarryOS_guest udp recv ret=-1 errno=110' "${log_file}" || true)"
send_count="$(grep -Ec 'AICP StarryOS_guest udp send begin' "${log_file}" || true)"
expected_sends="$((iterations + 1))"
if (( send_count > expected_sends )); then
  retries="$((send_count - expected_sends))"
else
  retries=0
fi
status_ok="$(grep -Ec 'AICP_STARRY_(NATIVE_)?STATUS seq=' "${log_file}" || true)"
done_ok="$(sed -n 's/.*AICP_STARRY_DONE ok=\([0-9][0-9]*\).*/\1/p' "${log_file}" | tail -n 1)"
done_failed="$(sed -n 's/.*AICP_STARRY_DONE .*failed=\([0-9][0-9]*\).*/\1/p' "${log_file}" | tail -n 1)"

cat > "${result_dir}/summary.txt" <<EOF
iterations=${iterations}
drop_every=${drop_every}
fault_drops=${fault_drops}
client_timeouts=${timeouts}
client_retries=${retries}
client_datagrams_sent=${send_count}
server_duplicate_replays=${duplicates}
server_out_of_order_rejections=${out_of_order}
client_stale_sequence_rejected=${stale_rejected}
status_responses=${status_ok}
done_ok=${done_ok:-0}
done_failed=${done_failed:-unknown}
recovery_success_rate=$(python3 -c 'import sys; print(f"{int(sys.argv[1]) / int(sys.argv[2]):.6f}")' "${done_ok:-0}" "${iterations}")
EOF

cat "${result_dir}/summary.txt"
if [[ "${done_ok:-0}" != "${iterations}" || "${done_failed:-1}" != "0" || "${fault_drops}" == "0" || "${duplicates}" == "0" || "${out_of_order}" == "0" || "${stale_rejected}" == "0" ]]; then
  echo "ERROR: UDP fault recovery did not meet expectations" >&2
  exit 1
fi

echo "[ai-rtos] PASS: UDP fault recovery artifacts are in ${result_dir}"
