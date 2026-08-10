#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
#
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  scripts/ai-rtos/run_rtos_boot_smoke.sh <freertos|zephyr|arceos-aicp> [timeout_seconds]

Builds/runs AxVisor under QEMU with the selected RTOS guest config and
captures the serial log under tmp/ai-rtos/logs. The command succeeds when the
expected guest boot markers are observed.
EOF
}

if [[ $# -lt 1 || $# -gt 2 ]]; then
  usage >&2
  exit 2
fi

guest="$1"
timeout_s="${2:-20}"
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
log_dir="${repo_root}/tmp/ai-rtos/logs"
mkdir -p "${log_dir}"

case "${guest}" in
  freertos)
    exec "${repo_root}/scripts/ai-rtos/run_freertos_aicp_guest_smoke.sh" "${timeout_s}"
    ;;
  zephyr)
    vmconfig="tmp/ai-rtos/zephyr-qemu.generated.toml"
    qemu_config="os/axvisor/configs/qemu/qemu-aarch64.toml"
    markers=("Booting Zephyr OS")
    ;;
  arceos-aicp)
    vmconfig="tmp/ai-rtos/arceos-aicp-qemu.generated.toml"
    qemu_config="os/axvisor/configs/qemu/qemu-aarch64-aicp-net.toml"
    markers=(
      "registered virtio network device"
      "DHCP acquired address"
      "AICP ArceOS RTOS server listening"
    )
    ;;
  *)
    usage >&2
    exit 2
    ;;
esac

"${repo_root}/scripts/ai-rtos/setup_qemu_rtos.sh" "${guest}"

log_file="${log_dir}/${guest}-boot-$(date +%Y%m%d-%H%M%S).log"
echo "[ai-rtos] Running ${guest} boot smoke; log: ${log_file}"

set +e
(
  cd "${repo_root}"
  timeout "${timeout_s}s" cargo xtask axvisor qemu \
    --config os/axvisor/configs/board/qemu-aarch64.toml \
    --qemu-config "${qemu_config}" \
    --vmconfigs "${vmconfig}"
) 2>&1 | tee "${log_file}"
status=${PIPESTATUS[0]}
set -e

for marker in "${markers[@]}"; do
  if ! grep -q "${marker}" "${log_file}"; then
    echo "[ai-rtos] FAIL: marker '${marker}' not observed (command status=${status})" >&2
    exit 1
  fi
done

echo "[ai-rtos] PASS: observed ${#markers[@]} marker(s)"
