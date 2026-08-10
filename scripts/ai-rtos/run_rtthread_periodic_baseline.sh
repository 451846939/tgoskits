#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
#
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
revision="${RTTHREAD_REVISION:-v5.2.1}"
source_dir="${RTTHREAD_SOURCE_DIR:-${repo_root}/tmp/rt-thread-${revision}}"
bsp_dir="${source_dir}/bsp/qemu-virt64-aarch64"
venv="${repo_root}/tmp/rtthread-venv"
result_dir="${repo_root}/tmp/ai-rtos/results/rtthread-periodic-$(date +%Y%m%d-%H%M%S)"
app_source="${repo_root}/apps/ai-rtos-demo/rtthread-baseline/main.c"
toolchain_version="14.3.rel1"
source "${repo_root}/scripts/ai-rtos/lib/host_tools.sh"
source "${repo_root}/scripts/ai-rtos/lib/third_party_source_guard.sh"

mkdir -p "${result_dir}"

if [[ ! -d "${source_dir}/.git" ]]; then
  git clone --depth 1 --branch "${revision}" \
    https://github.com/RT-Thread/rt-thread.git "${source_dir}"
fi

verify_rtthread_source() {
  third_party_assert_git_source "RT-Thread" "${source_dir}" "${revision}"
}

verify_rtthread_on_exit() {
  local status=$?
  trap - EXIT
  if ! verify_rtthread_source; then
    exit 1
  fi
  exit "${status}"
}

verify_rtthread_source
trap verify_rtthread_on_exit EXIT

if [[ ! -x "${venv}/bin/scons" ]]; then
  python3 -m venv "${venv}"
  "${venv}/bin/pip" install 'scons==4.8.1' 'kconfiglib==14.1.0'
elif ! "${venv}/bin/python" -c 'import kconfiglib' >/dev/null 2>&1; then
  "${venv}/bin/pip" install 'kconfiglib==14.1.0'
fi

cross_prefix="$(aicp_resolve_or_install_aarch64_none_elf \
  "${repo_root}" "${toolchain_version}" RTTHREAD_CC_PREFIX)"

if [[ ! -x "${cross_prefix}gcc" ]] && ! command -v "${cross_prefix}gcc" >/dev/null 2>&1; then
  echo "ERROR: ${cross_prefix}gcc was not found" >&2
  exit 1
fi

run_mode() {
  local mode="$1"
  local build_dir="${repo_root}/tmp/rtthread-build-${mode}"
  local log_file="${result_dir}/${mode}.log"

  rm -rf "${build_dir}"
  mkdir -p "${build_dir}"
  cp -R "${bsp_dir}/." "${build_dir}/"

  # The BSP's Kconfig uses a relative ../../ RT-Thread root. Because this
  # script builds from a disposable copy, point it at the downloaded source
  # and regenerate rtconfig.h so newly added Kconfig defaults are included.
  sed -i.bak "s|^RTT_DIR := ../../$|RTT_DIR := ${source_dir}|" "${build_dir}/Kconfig"
  rm -f "${build_dir}/Kconfig.bak"
  (
    cd "${build_dir}"
    RTT_ROOT="${source_dir}" "${venv}/bin/scons" --pyconfig-silent
  )

  cp "${app_source}" "${build_dir}/applications/main.c"

  if [[ "${mode}" == "stress" ]]; then
    sed -i.bak '1i\
#define AICP_BASELINE_STRESS 1\
' "${build_dir}/applications/main.c"
    rm -f "${build_dir}/applications/main.c.bak"
  fi

  echo "[ai-rtos] Building RT-Thread baseline mode=${mode} revision=${revision}"
  (
    cd "${build_dir}"
    RTT_ROOT="${source_dir}" RTT_EXEC_PATH="$(dirname "$(command -v "${cross_prefix}gcc" 2>/dev/null || echo "${cross_prefix}gcc")")" \
      RTT_CC_PREFIX="${cross_prefix}" "${venv}/bin/scons" -j"$(sysctl -n hw.ncpu 2>/dev/null || echo 4)"
  )

  echo "[ai-rtos] Running RT-Thread baseline mode=${mode}"
  set +e
  timeout 35s qemu-system-aarch64 \
    -M virt,gic-version=2 -cpu cortex-a53 -m 128M -smp 4 \
    -kernel "${build_dir}/rtthread.bin" -nographic -no-reboot \
    2>&1 | tee "${log_file}"
  local command_status=${PIPESTATUS[0]}
  set -e

  if ! grep -q "AICP_RTTHREAD_BASELINE_DONE mode=${mode}" "${log_file}"; then
    echo "ERROR: RT-Thread ${mode} marker missing (status=${command_status})" >&2
    exit 1
  fi
  if [[ ${command_status} -ne 0 && ${command_status} -ne 124 ]]; then
    echo "ERROR: RT-Thread ${mode} QEMU failed (status=${command_status})" >&2
    exit 1
  fi
}

run_mode idle
run_mode stress

python3 - "${result_dir}" <<'PY'
# SPDX-License-Identifier: Apache-2.0
import pathlib
import re
import sys

result_dir = pathlib.Path(sys.argv[1])
pattern = re.compile(
    r"AICP_RTTHREAD_RESULT mode=(?P<mode>\w+) "
    r"key=(?P<key>[a-z0-9_]+) value=(?P<value>\d+)"
)
required_keys = (
    "samples",
    "period_ns",
    "avg_abs_jitter_ns",
    "p99_abs_jitter_ns",
    "max_abs_jitter_ns",
    "missed_deadlines",
    "avg_interval_jitter_ns",
    "p99_interval_jitter_ns",
    "max_interval_jitter_ns",
)

lines = []
for mode in ("idle", "stress"):
    text = (result_dir / f"{mode}.log").read_bytes().replace(b"\x00", b"").decode(errors="replace")
    values = {
        match.group("key"): match.group("value")
        for match in pattern.finditer(text)
        if match.group("mode") == mode
    }
    missing = [key for key in required_keys if key not in values]
    if missing:
        raise SystemExit(f"missing RT-Thread {mode} fields: {', '.join(missing)}")
    lines.extend(f"{mode}_{key}={values[key]}" for key in required_keys)

(result_dir / "rtthread-periodic.summary.txt").write_text("\n".join(lines) + "\n")
print("\n".join(lines))
PY

echo "[ai-rtos] PASS: RT-Thread artifacts are in ${result_dir}"
trap - EXIT
verify_rtthread_source
