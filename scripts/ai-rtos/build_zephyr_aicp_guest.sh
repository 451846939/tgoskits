#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
#
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  scripts/ai-rtos/build_zephyr_aicp_guest.sh [board]

Builds the Zephyr AICP RTOS guest application. The default board is
qemu_cortex_a53. The script expects an existing Zephyr workspace:

  export ZEPHYR_BASE=/path/to/zephyr
  export WEST=/path/to/venv/bin/west        # optional
  export ZEPHYR_BUILD_DIR=/path/to/build    # optional
  export ZEPHYR_REQUIRED_REF=v4.4.0          # optional, defaults to v4.4.0
  export ZEPHYR_TOOLCHAIN_VARIANT=cross-compile
  export CROSS_COMPILE=/path/to/aarch64-linux-musl-
  export AICP_ZEPHYR_PROFILE=e1000          # e1000 or axvisor-virtio

Output:
  $ZEPHYR_BUILD_DIR/zephyr/zephyr.bin
  $ZEPHYR_BUILD_DIR/zephyr/zephyr.elf

The Zephyr source tree must be at the exact requested Git tag or commit and
must have a clean worktree. This script never patches third-party sources.
EOF
}

if [[ $# -gt 1 || "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

board="${1:-qemu_cortex_a53}"
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
app_dir="${repo_root}/apps/ai-rtos-demo/zephyr"
build_dir="${ZEPHYR_BUILD_DIR:-${app_dir}/build}"
required_ref="${ZEPHYR_REQUIRED_REF:-v4.4.0}"
west_bin="${WEST:-west}"
profile="${AICP_ZEPHYR_PROFILE:-e1000}"
source "${repo_root}/scripts/ai-rtos/lib/third_party_source_guard.sh"

if ! command -v "${west_bin}" >/dev/null 2>&1; then
  echo "ERROR: west was not found: ${west_bin}" >&2
  exit 1
fi

if [[ -z "${ZEPHYR_BASE:-}" || ! -d "${ZEPHYR_BASE}" ]]; then
  echo "ERROR: ZEPHYR_BASE must point to an initialized Zephyr source tree." >&2
  exit 1
fi

zephyr_workspace="$(cd "${ZEPHYR_BASE}/.." && pwd)"

verify_zephyr_sources() {
  third_party_assert_git_source "Zephyr" "${ZEPHYR_BASE}" "${required_ref}"
  third_party_assert_nested_git_clean "Zephyr workspace" "${zephyr_workspace}"
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
actual_commit="$(git -C "${ZEPHYR_BASE}" rev-parse HEAD)"
trap verify_zephyr_on_exit EXIT

cmake_args=()
case "${profile}" in
  e1000)
    ;;
  axvisor-virtio)
    cmake_args+=(
      "-DAICP_ZEPHYR_VIRTIO_LEGACY=ON"
      "-DEXTRA_CONF_FILE=${app_dir}/axvisor-virtio.conf"
      "-DDTC_OVERLAY_FILE=${app_dir}/axvisor-virtio.overlay"
    )
    ;;
  *)
    echo "ERROR: AICP_ZEPHYR_PROFILE must be e1000 or axvisor-virtio." >&2
    exit 2
    ;;
esac

echo "[ai-rtos] Building Zephyr AICP guest for board=${board} profile=${profile}"
echo "[ai-rtos] Zephyr source: ref=${required_ref} commit=${actual_commit} clean=yes"
if [[ ${#cmake_args[@]} -eq 0 ]]; then
  "${west_bin}" build -p always -b "${board}" "${app_dir}" -d "${build_dir}"
else
  "${west_bin}" build -p always -b "${board}" "${app_dir}" -d "${build_dir}" -- "${cmake_args[@]}"
fi

trap - EXIT
verify_zephyr_sources
echo "[ai-rtos] Build complete:"
echo "  ${build_dir}/zephyr/zephyr.bin"
echo "  ${build_dir}/zephyr/zephyr.elf"
