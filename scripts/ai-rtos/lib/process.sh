#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
#
# SPDX-License-Identifier: Apache-2.0

# Replaces the current subshell with a command running in its own session.
# `setsid` is provided by util-linux on most Linux hosts; Perl's POSIX module is
# the portable fallback used on hosts where the standalone utility is absent.
aicp_exec_new_session() {
  if command -v setsid >/dev/null 2>&1; then
    exec setsid "$@"
  fi
  if command -v perl >/dev/null 2>&1; then
    exec perl -MPOSIX -e 'POSIX::setsid() >= 0 or die "setsid: $!\n"; exec @ARGV or die "exec: $!\n"' -- "$@"
  fi
  exec "$@"
}

# Terminates a process and all descendants, deepest children first. QEMU is
# normally launched through cargo xtask, so killing only the wrapper process can
# leave the QEMU child and its file locks behind. A process started with
# `aicp_exec_new_session` is also terminated by process group, which remains
# reliable when host process-enumeration APIs are restricted.
aicp_cleanup_process_tree() {
  local root_pid="${1:-}"
  if [[ -z "${root_pid}" ]]; then
    return 0
  fi

  local descendants=()
  local pending=("${root_pid}")
  local pid child idx
  while ((${#pending[@]})); do
    pid="${pending[0]}"
    pending=("${pending[@]:1}")
    while read -r child; do
      [[ -z "${child}" ]] && continue
      descendants+=("${child}")
      pending+=("${child}")
    done < <(pgrep -P "${pid}" 2>/dev/null || true)
  done

  kill -TERM -- "-${root_pid}" 2>/dev/null || true
  for ((idx = ${#descendants[@]} - 1; idx >= 0; idx--)); do
    kill "${descendants[idx]}" 2>/dev/null || true
  done
  kill "${root_pid}" 2>/dev/null || true

  local tracked=("${root_pid}")
  if ((${#descendants[@]})); then
    tracked=("${descendants[@]}" "${root_pid}")
  fi
  local deadline=$((SECONDS + 5))
  (
    while ((SECONDS < deadline)); do
      sleep 0.1
    done
    kill -KILL -- "-${root_pid}" 2>/dev/null || true
    for pid in "${tracked[@]}"; do
      kill -KILL "${pid}" 2>/dev/null || true
    done
  ) &
  local watchdog_pid=$!

  # The root process is normally a direct child of the calling script. Reap it
  # immediately instead of polling with kill -0, which also reports zombies as
  # alive. The watchdog bounds this wait even when a wrapper ignores SIGTERM.
  wait "${root_pid}" 2>/dev/null || true
  kill "${watchdog_pid}" 2>/dev/null || true
  wait "${watchdog_pid}" 2>/dev/null || true

  kill -KILL -- "-${root_pid}" 2>/dev/null || true
  for pid in "${tracked[@]}"; do
    kill -KILL "${pid}" 2>/dev/null || true
  done
}

# Prints only QEMU processes that currently have an image open. macOS indexing
# and file-provider services may briefly open large images read-only; they do
# not prevent the next QEMU instance from using the image and must not be
# treated as stale virtual-machine processes.
aicp_image_qemu_users() {
  local image="$1"
  local pid command_name

  [[ -e "${image}" ]] || return 1
  while IFS= read -r pid; do
    [[ -n "${pid}" ]] || continue
    command_name="$(ps -p "${pid}" -o comm= 2>/dev/null || true)"
    case "$(basename "${command_name}")" in
      qemu|qemu-kvm|qemu-system-*)
        printf '%s %s\n' "${pid}" "${command_name}"
        ;;
    esac
  done < <(lsof -nP -t -- "${image}" 2>/dev/null | sort -u)
}

aicp_wait_for_qemu_image_release() {
  local image="$1"
  local timeout_s="${2:-20}"
  local deadline=$((SECONDS + timeout_s))
  local users

  while ((SECONDS < deadline)); do
    users="$(aicp_image_qemu_users "${image}")"
    if [[ -z "${users}" ]]; then
      return 0
    fi
    sleep 1
  done

  echo "[ai-rtos] FAIL: QEMU still has the image open: ${image}" >&2
  aicp_image_qemu_users "${image}" >&2 || true
  return 1
}
