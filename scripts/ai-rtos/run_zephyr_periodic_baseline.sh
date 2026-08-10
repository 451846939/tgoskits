#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
#
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  scripts/ai-rtos/run_zephyr_periodic_baseline.sh

Builds and runs the native Zephyr 20 ms periodic-task baseline in idle and
CPU-stress modes. Results are saved under:

  tmp/ai-rtos/results/zephyr-periodic-<timestamp>

The script uses an isolated Python virtual environment and the existing
AArch64 cross compiler. It downloads the pinned Zephyr source only on the
first run.

Optional environment variables:
  ZEPHYR_REVISION       Zephyr git revision (default: v4.2.0)
  ZEPHYR_CROSS_COMPILE  Cross-compiler prefix
  ZEPHYR_WORKSPACE      Revision-isolated workspace directory
  ZEPHYR_SOURCE_URL     Git source URL or local mirror
  ZEPHYR_VENV           Existing or newly-created Python environment
  ZEPHYR_SKIP_EXPORT    Set to 1 when user-level CMake registration is unavailable
  ZEPHYR_BASELINE_TIMEOUT_SECONDS  Per-mode QEMU timeout (default: 90)
EOF
}

if [[ $# -ne 0 ]]; then
  usage >&2
  exit 2
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
source "${repo_root}/scripts/ai-rtos/lib/host_tools.sh"
revision="${ZEPHYR_REVISION:-v4.2.0}"
cross_compile="$(aicp_resolve_cross_prefix ZEPHYR_CROSS_COMPILE aarch64-linux-musl-)"
revision_key="$(aicp_revision_key "${revision}")"
workspace="${ZEPHYR_WORKSPACE:-${repo_root}/tmp/zephyrproject-${revision_key}}"
zephyr_base="${workspace}/zephyr"
source_url="${ZEPHYR_SOURCE_URL:-https://github.com/zephyrproject-rtos/zephyr.git}"
venv="${ZEPHYR_VENV:-${repo_root}/tmp/zephyr-venv-${revision_key}}"
result_dir="${repo_root}/tmp/ai-rtos/results/zephyr-periodic-$(date +%Y%m%d-%H%M%S)"
app_dir="${repo_root}/apps/ai-rtos-demo/zephyr-baseline"
run_timeout_seconds="${ZEPHYR_BASELINE_TIMEOUT_SECONDS:-90}"
source "${repo_root}/scripts/ai-rtos/lib/third_party_source_guard.sh"

if [[ ! "${run_timeout_seconds}" =~ ^[1-9][0-9]*$ ]]; then
  echo "ERROR: ZEPHYR_BASELINE_TIMEOUT_SECONDS must be a positive integer" >&2
  exit 2
fi

find_python() {
  local candidate="${PYTHON:-python3}"

  if command -v "${candidate}" >/dev/null 2>&1 && \
     "${candidate}" -c 'import sys; raise SystemExit(sys.version_info < (3, 10))'; then
    command -v "${candidate}"
    return 0
  fi

  echo "ERROR: Zephyr v4.2.0 需要 Python 3.10 或更高版本；可通过 PYTHON 指定" >&2
  return 1
}

python_bin="$(find_python)"
mkdir -p "${workspace}" "${result_dir}"

if [[ ! -x "${venv}/bin/west" ]]; then
  echo "[ai-rtos] Creating Zephyr Python environment with ${python_bin}"
  "${python_bin}" -m venv "${venv}"
  "${venv}/bin/pip" install --upgrade pip west
fi

if [[ ! -d "${zephyr_base}/.git" ]]; then
  echo "[ai-rtos] Cloning Zephyr revision=${revision}"
  git clone --depth 1 --branch "${revision}" \
    "${source_url}" "${zephyr_base}"
fi

verify_zephyr_sources() {
  third_party_assert_git_source "Zephyr" "${zephyr_base}" "${revision}"
  third_party_assert_nested_git_clean "Zephyr workspace" "${workspace}"
}

verify_zephyr_on_exit() {
  local status=$?
  trap - EXIT
  if ! verify_zephyr_sources; then
    exit 1
  fi
  exit "${status}"
}

verify_zephyr_sources
trap verify_zephyr_on_exit EXIT

"${venv}/bin/pip" install -r "${zephyr_base}/scripts/requirements-base.txt"

if [[ ! -d "${workspace}/.west" ]]; then
  (
    cd "${workspace}"
    "${venv}/bin/west" init -l zephyr
  )
fi

export ZEPHYR_BASE="${zephyr_base}"
export ZEPHYR_TOOLCHAIN_VARIANT=cross-compile
export CROSS_COMPILE="${cross_compile}"

if [[ "${ZEPHYR_SKIP_EXPORT:-0}" != "1" ]]; then
  "${venv}/bin/west" zephyr-export
fi

run_mode() {
  local mode="$1"
  local build_dir="${workspace}/build-aicp-${mode}"
  local log_file="${result_dir}/${mode}.log"

  echo "[ai-rtos] Building Zephyr periodic baseline mode=${mode}"
  if [[ "${mode}" == "stress" ]]; then
    "${venv}/bin/west" build -p always -b qemu_cortex_a53 \
      "${app_dir}" -d "${build_dir}" -- -DEXTRA_CONF_FILE=stress.conf
  else
    "${venv}/bin/west" build -p always -b qemu_cortex_a53 \
      "${app_dir}" -d "${build_dir}"
  fi

  echo "[ai-rtos] Running Zephyr periodic baseline mode=${mode}"
  set +e
  timeout "${run_timeout_seconds}s" "${venv}/bin/west" build -d "${build_dir}" -t run \
    2>&1 | tee "${log_file}"
  local command_status=${PIPESTATUS[0]}
  set -e

  if ! grep -q "AICP_ZEPHYR_BASELINE_DONE mode=${mode}" "${log_file}"; then
    echo "ERROR: Zephyr ${mode} baseline marker missing (status=${command_status})" >&2
    exit 1
  fi
  if [[ ${command_status} -ne 0 && ${command_status} -ne 124 ]]; then
    echo "ERROR: Zephyr ${mode} QEMU run failed (status=${command_status})" >&2
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
    r"AICP_ZEPHYR_BASELINE_DONE mode=(?P<mode>\w+) "
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

(result_dir / "zephyr-periodic.summary.txt").write_text("\n".join(lines) + "\n")
PY

cat "${result_dir}/zephyr-periodic.summary.txt"
echo "[ai-rtos] PASS: Zephyr periodic baseline artifacts are in ${result_dir}"
trap - EXIT
verify_zephyr_sources
