#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
# SPDX-License-Identifier: Apache-2.0

# Extract the last machine-readable value emitted by a guest runner.
aicp_runner_output_value() {
  local runner_output="$1"
  local key="$2"

  LC_ALL=C sed -n "s/^${key}=//p" "${runner_output}" | tail -n 1
}

# Archive the independent QEMU and Linux console logs returned by a dual-guest
# runner. Keeping both sources prevents guest console filename ordering from
# hiding hypervisor and RTOS timing events.
aicp_archive_dual_guest_logs() {
  local runner_output="$1"
  local result_dir="$2"
  local qemu_log
  local linux_console_log

  if [[ ! -f "${runner_output}" ]]; then
    echo "ERROR: runner output does not exist: ${runner_output}" >&2
    return 1
  fi

  qemu_log="$(aicp_runner_output_value "${runner_output}" log)"
  linux_console_log="$(aicp_runner_output_value "${runner_output}" linux_console_log)"

  if [[ -z "${qemu_log}" || ! -f "${qemu_log}" ]]; then
    echo "ERROR: runner did not return a valid QEMU log path" >&2
    return 1
  fi
  if [[ -z "${linux_console_log}" || ! -f "${linux_console_log}" ]]; then
    echo "ERROR: runner did not return a valid Linux console log path" >&2
    return 1
  fi
  if [[ "${qemu_log}" == "${linux_console_log}" ]]; then
    echo "ERROR: QEMU and Linux console logs must be different files" >&2
    return 1
  fi

  mkdir -p "${result_dir}"
  cp "${qemu_log}" "${result_dir}/qemu.log"
  cp "${linux_console_log}" "${result_dir}/linux-console.log"
  cat "${result_dir}/qemu.log" "${result_dir}/linux-console.log" > "${result_dir}/run.log"
}
