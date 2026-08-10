#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
#
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
log_dir="${repo_root}/tmp/ai-rtos/logs"
mkdir -p "${log_dir}"

log_file="${log_dir}/arceos-aicp-qemu-$(date +%Y%m%d-%H%M%S).log"
echo "[ai-rtos] Booting ArceOS AICP server in QEMU; log: ${log_file}"

(
  cd "${repo_root}"
  cargo xtask arceos qemu \
    -p arceos-aicp-server \
    --arch aarch64 \
    --config apps/arceos/build-aarch64-unknown-none-softfloat.toml \
    --qemu-config apps/arceos/aicp-server/qemu-aarch64.toml
) 2>&1 | tee "${log_file}"

if grep -q "AICP ArceOS RTOS server listening on 0.0.0.0:8800" "${log_file}"; then
  echo "[ai-rtos] PASS: ArceOS AICP server boot marker observed"
  exit 0
fi

echo "[ai-rtos] FAIL: ArceOS AICP server boot marker not observed" >&2
exit 1
