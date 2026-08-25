#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
script_dir="${repo_root}/scripts/ai-rtos"

resolve_tool() {
  local env_name="$1"
  local tool_name="$2"
  local configured="${!env_name:-}"

  if [[ -n "${configured}" ]]; then
    if command -v "${configured}" >/dev/null 2>&1; then
      command -v "${configured}"
      return 0
    fi
    if [[ -x "${configured}" ]]; then
      printf '%s\n' "${configured}"
      return 0
    fi
    return 1
  fi

  command -v "${tool_name}"
}

usage() {
  cat <<'EOF'
Usage:
  scripts/ai-rtos/aicp.sh doctor
  scripts/ai-rtos/aicp.sh prepare
  scripts/ai-rtos/aicp.sh smoke [iterations] [ai|fixed] [boot_timeout_seconds]

This entry point validates one focused integration path:
Linux (two vCPUs) -> AICP TCP/IP -> ArceOS control guest, inside AxVisor QEMU.
EOF
}

doctor() {
  local missing=0 tool resolved
  for tool in cargo cpio gzip perl qemu-system-aarch64; do
    if resolved="$(command -v "${tool}" 2>/dev/null)"; then
      printf '  %-22s %s\n' "${tool}" "${resolved}"
    else
      printf '  %-22s missing\n' "${tool}"
      missing=1
    fi
  done
  for tool in DEBUGFS:debugfs E2FSCK:e2fsck; do
    local env_name="${tool%%:*}"
    local command_name="${tool#*:}"
    if resolved="$(resolve_tool "${env_name}" "${command_name}" 2>/dev/null)"; then
      printf '  %-22s %s\n' "${env_name}" "${resolved}"
    else
      printf '  %-22s missing\n' "${env_name}"
      missing=1
    fi
  done
  ((missing == 0)) || return 1
}

command_name="${1:---help}"
case "${command_name}" in
  -h|--help|help)
    usage
    ;;
  doctor)
    (($# == 1)) || { usage >&2; exit 2; }
    doctor
    ;;
  prepare)
    (($# == 1)) || { usage >&2; exit 2; }
    cd "${repo_root}"
    exec cargo xtask image pull qemu-aarch64 --extract-dir tmp/images
    ;;
  smoke)
    (($# >= 1 && $# <= 4)) || { usage >&2; exit 2; }
    exec "${script_dir}/runners/run_axvisor_dual_guest_aicp.sh" "${@:2}"
    ;;
  *)
    usage >&2
    exit 2
    ;;
esac
