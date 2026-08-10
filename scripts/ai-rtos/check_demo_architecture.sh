#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
#
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
demo_root="${repo_root}/apps/ai-rtos-demo"
failed=0
checked=0

fail() {
  echo "ERROR: $*" >&2
  failed=1
}

require_file() {
  local path="$1"
  checked=$((checked + 1))
  if [[ ! -f "${demo_root}/${path}" ]]; then
    fail "required demo file is missing: apps/ai-rtos-demo/${path}"
  fi
}

require_absent() {
  local path="$1"
  checked=$((checked + 1))
  if [[ -e "${demo_root}/${path}" ]]; then
    fail "obsolete or duplicated path must not exist: apps/ai-rtos-demo/${path}"
  fi
}

require_contains() {
  local path="$1"
  local pattern="$2"
  local description="$3"
  checked=$((checked + 1))
  if ! rg -q -- "${pattern}" "${demo_root}/${path}"; then
    fail "${description}: apps/ai-rtos-demo/${path}"
  fi
}

require_repo_contains() {
  local path="$1"
  local pattern="$2"
  local description="$3"
  checked=$((checked + 1))
  if ! rg -q -- "${pattern}" "${repo_root}/${path}"; then
    fail "${description}: ${path}"
  fi
}

require_repo_absent() {
  local path="$1"
  local pattern="$2"
  local description="$3"
  checked=$((checked + 1))
  if rg -q -- "${pattern}" "${repo_root}/${path}"; then
    fail "${description}: ${path}"
  fi
}

for path in \
  aicp/aicp.h \
  aicp/aicp_stream.h \
  aicp/aicp_datagram.h \
  aicp/aicp_posix_stream.h \
  aicp/aicp_client.c \
  rtos-core/aicp_service.c \
  rtos-core/control_loop.c \
  ai-model/control_policy.c \
  linux-client/main.c \
  linux-init/aicp_init.c \
  linux-init/starry_profile.h \
  rtthread-aicp/main.c \
  zephyr/src/main.c \
  freertos/main.c; do
  require_file "${path}"
done

for path in \
  linux-client/aicp_client.c \
  model-image \
  image-classifier \
  rt_compat_baseline; do
  require_absent "${path}"
done

require_contains linux-client/main.c 'aicp_client_session_transact_control' \
  "C client must use the shared AICP transaction core"
require_contains linux-init/aicp_init.c 'aicp_client_session_transact_control' \
  "Linux/Starry PID 1 must use the shared AICP transaction core"
require_contains Makefile '\$\(AICP_STARRY_DEFS\)' \
  "Starry userspace build must consume the generated network and transport definitions"
require_contains yolov8-onnx-cpu/src/main.cc 'aicp_client_session_transact_control' \
  "C++ YOLOv8 client must use the shared AICP transaction core"
require_contains rtthread-aicp/main.c '#include "aicp_service.h"' \
  "RT-Thread glue must use the shared RTOS service state machine"
require_contains zephyr/src/main.c '#include "aicp_service.h"' \
  "Zephyr glue must use the shared RTOS service state machine"
require_contains freertos/main.c '#include "aicp_service.h"' \
  "FreeRTOS glue must use the shared RTOS service state machine"
require_repo_contains net/ax-net/src/device/ethernet.rs \
  'pub fn new_polling' \
  "poll-only networking must expose a dedicated device mode"
require_repo_contains net/ax-net/src/lib.rs \
  'EthernetDevice::new_polling' \
  "poll-only networking must retain a device IRQ acknowledgement path"
require_repo_absent os/axvisor/src/manager.rs \
  'set_aarch64_passthrough_irq_routes_enabled\(vm_id, true\)' \
  "VM startup must not unmask passthrough IRQs before the guest driver enables them"
require_repo_absent scripts/ai-rtos/run_axvisor_starry_rtos_aicp.sh \
  '^  \["/"\],$' \
  "dual-guest configs must assign explicit devices instead of claiming the host device tree root"
require_repo_contains scripts/ai-rtos/run_zephyr_periodic_baseline.sh \
  'ZEPHYR_BASELINE_TIMEOUT_SECONDS' \
  "Zephyr periodic baseline timeout must be configurable for loaded validation hosts"

checked=$((checked + 1))
if rg -n '__ZEPHYR__|__RTTHREAD__|AICP_FREERTOS|Starry|Linux' \
  "${demo_root}/aicp" \
  "${demo_root}/rtos-core" \
  "${demo_root}/ai-model"; then
  fail "OS-specific branches are not allowed in shared protocol, service, or model code"
fi

checked=$((checked + 1))
protocol_definitions="$(rg -l 'static inline uint16_t aicp_crc16_ccitt_update' "${demo_root}" || true)"
if [[ "${protocol_definitions}" != "${demo_root}/aicp/aicp.h" ]]; then
  fail "AICP CRC implementation must have exactly one definition in aicp/aicp.h"
fi

checked=$((checked + 1))
if find "${demo_root}" \
  \( -type d \( -name build -o -name target -o -name install -o -name third_party \) -prune \) -o \
  -type d \
  \( -name FreeRTOS-Kernel -o -name FreeRTOS-Plus-TCP -o -name rt-thread -o -name zephyrproject \) \
  -print -quit | grep -q .; then
  fail "third-party RTOS source trees must stay under tmp/, not apps/ai-rtos-demo/"
fi

if (( failed != 0 )); then
  exit 1
fi

echo "[ai-rtos] PASS: demo architecture boundaries checked (${checked} assertions)"
