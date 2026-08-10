#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
stamp="$(date +%Y%m%d-%H%M%S)"
result_dir="${repo_root}/tmp/ai-rtos/results/freertos-periodic-${stamp}"
mkdir -p "${result_dir}"

for tool in qemu-system-aarch64 cmake ninja; do
  if ! command -v "${tool}" >/dev/null 2>&1; then
    echo "ERROR: 缺少工具：${tool}" >&2
    exit 1
  fi
done

run_mode() {
  local mode="$1"
  local build_dir="${repo_root}/tmp/ai-rtos/build-freertos-baseline-${mode}"
  local log_file="${result_dir}/${mode}.log"
  local stress=OFF
  if [[ "${mode}" == "stress" ]]; then
    stress=ON
  fi

  echo "[ai-rtos] 构建 FreeRTOS 周期基线 mode=${mode}"
  AICP_FREERTOS_BASELINE=ON \
    AICP_FREERTOS_STRESS="${stress}" \
    FREERTOS_BUILD_DIR="${build_dir}" \
    "${repo_root}/scripts/ai-rtos/build_freertos_aicp_guest.sh"

  echo "[ai-rtos] 直接启动 QEMU FreeRTOS 周期基线 mode=${mode}"
  set +e
  timeout 30s qemu-system-aarch64 \
    -machine virt,gic-version=3,virtualization=on \
    -cpu cortex-a72 \
    -smp 1 \
    -m 3g \
    -nographic \
    -monitor none \
    -device "loader,file=${build_dir}/aicp-freertos.elf,cpu-num=0" \
    >"${log_file}" 2>&1
  local command_status=$?
  set -e

  if ! grep -Fq "AICP_FREERTOS_BASELINE_DONE mode=${mode}" "${log_file}"; then
    echo "ERROR: FreeRTOS ${mode} 周期基线完成标记缺失（status=${command_status}）" >&2
    tail -n 120 "${log_file}" >&2 || true
    exit 1
  fi
  if [[ ${command_status} -ne 0 && ${command_status} -ne 124 ]]; then
    echo "ERROR: FreeRTOS ${mode} QEMU 退出异常（status=${command_status}）" >&2
    exit 1
  fi
}

run_mode idle
run_mode stress

python3 - "${result_dir}" <<'PY'
import pathlib
import re
import sys

result_dir = pathlib.Path(sys.argv[1])
pattern = re.compile(
    r"AICP_FREERTOS_BASELINE_DONE mode=(?P<mode>\w+) "
    r"samples=(?P<samples>\d+) period_ns=(?P<period_ns>\d+) "
    r"avg_abs_jitter_ns=(?P<avg>\d+) p99_abs_jitter_ns=(?P<p99>\d+) "
    r"max_abs_jitter_ns=(?P<max>\d+) missed_deadlines=(?P<missed>\d+)"
)

lines = []
for mode in ("idle", "stress"):
    text = (result_dir / f"{mode}.log").read_text(errors="replace")
    match = pattern.search(text)
    if match is None:
        raise SystemExit(f"missing summary in {mode}.log")
    values = match.groupdict()
    lines.extend(
        [
            f"{mode}_samples={values['samples']}",
            f"{mode}_period_ns={values['period_ns']}",
            f"{mode}_avg_abs_jitter_ns={values['avg']}",
            f"{mode}_p99_abs_jitter_ns={values['p99']}",
            f"{mode}_max_abs_jitter_ns={values['max']}",
            f"{mode}_missed_deadlines={values['missed']}",
        ]
    )

summary = result_dir / "freertos-periodic.summary.txt"
summary.write_text("\n".join(lines) + "\n")
print(summary.read_text(), end="")
PY

echo "[ai-rtos] PASS：FreeRTOS 原生周期基线结果位于 ${result_dir}"
