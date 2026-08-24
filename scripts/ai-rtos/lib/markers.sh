#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
#
# SPDX-License-Identifier: Apache-2.0

aicp_print_log_tail() {
  local log_file="$1"
  local tail_lines="$2"
  tail -n "${tail_lines}" "${log_file}" >&2 || true
}

aicp_print_log_tails() {
  local tail_lines="$1"
  shift

  local log_file
  for log_file in "$@"; do
    echo "[ai-rtos] log tail: ${log_file}" >&2
    aicp_print_log_tail "${log_file}" "${tail_lines}"
  done
}

aicp_logs_contain_marker() {
  local marker="$1"
  shift

  local log_file
  for log_file in "$@"; do
    if LC_ALL=C grep -a -q "${marker}" "${log_file}" 2>/dev/null; then
      return 0
    fi
  done
  return 1
}

aicp_logs_match_regex() {
  local regex="$1"
  shift

  local log_file
  for log_file in "$@"; do
    if LC_ALL=C grep -a -E -q "${regex}" "${log_file}" 2>/dev/null; then
      return 0
    fi
  done
  return 1
}

aicp_logs_have_terminal_failure() {
  aicp_logs_match_regex \
    'AICP_RTTHREAD_RELIABILITY_SUMMARY passed=[0-9]+ failed=[1-9][0-9]*|AICP_LINUX_DONE ok=[0-9]+ failed=[1-9][0-9]*|AICP_YOLO_RUST_DONE ok=[0-9]+ failed=[1-9][0-9]*' \
    "$@"
}

aicp_protocol_event_regex() {
  case "$1" in
    hello)
      printf '%s\n' 'AICP([_[:alnum:]-]*)?_HELLO|AICP HELLO|type=HELLO'
      ;;
    control)
      printf '%s\n' 'AICP([_[:alnum:]-]*)?_CONTROL|CONTROL seq=|type=CONTROL_SET'
      ;;
    status)
      printf '%s\n' 'AICP([_[:alnum:]-]*)?_STATUS|STATUS seq=|type=STATUS|RX_FRAME type=3'
      ;;
    error)
      printf '%s\n' 'AICP([_[:alnum:]-]*)?_ERROR|ERROR seq=|type=ERROR|RX_FRAME type=4'
      ;;
    *)
      echo "[ai-rtos] unsupported AICP protocol event: $1" >&2
      return 2
      ;;
  esac
}

aicp_wait_for_marker_in_logs() {
  local marker="$1"
  local deadline="$2"
  local qemu_pid="$3"
  local tail_lines="$4"
  shift 4

  local log_files=("$@")
  if ((${#log_files[@]} == 0)); then
    echo "[ai-rtos] no logs supplied while waiting for marker: ${marker}" >&2
    return 2
  fi

  while ((SECONDS < deadline)); do
    if aicp_logs_contain_marker "${marker}" "${log_files[@]}"; then
      echo "[ai-rtos] observed: ${marker}"
      return 0
    fi
    if ! kill -0 "${qemu_pid}" 2>/dev/null; then
      echo "[ai-rtos] AxVisor exited before marker: ${marker}" >&2
      aicp_print_log_tails "${tail_lines}" "${log_files[@]}"
      return 1
    fi
    sleep 1
  done

  echo "[ai-rtos] timeout waiting for marker: ${marker}" >&2
  aicp_print_log_tails "${tail_lines}" "${log_files[@]}"
  return 1
}

aicp_wait_for_any_marker_in_logs() {
  local description="$1"
  local deadline="$2"
  local qemu_pid="$3"
  local tail_lines="$4"
  local log_count="$5"
  shift 5

  if ! [[ "${log_count}" =~ ^[1-9][0-9]*$ ]] || ((log_count > $#)); then
    echo "[ai-rtos] invalid log count while waiting for marker group: ${description}" >&2
    return 2
  fi

  local log_files=("${@:1:log_count}")
  shift "${log_count}"
  local markers=("$@")
  if ((${#markers[@]} == 0)); then
    echo "[ai-rtos] no markers supplied for group: ${description}" >&2
    return 2
  fi

  local marker
  while ((SECONDS < deadline)); do
    for marker in "${markers[@]}"; do
      if aicp_logs_contain_marker "${marker}" "${log_files[@]}"; then
        echo "[ai-rtos] observed: ${marker}"
        return 0
      fi
    done
    if ! kill -0 "${qemu_pid}" 2>/dev/null; then
      echo "[ai-rtos] AxVisor exited before marker group: ${description}" >&2
      aicp_print_log_tails "${tail_lines}" "${log_files[@]}"
      return 1
    fi
    sleep 1
  done

  echo "[ai-rtos] timeout waiting for marker group: ${description}" >&2
  aicp_print_log_tails "${tail_lines}" "${log_files[@]}"
  return 1
}

aicp_wait_for_protocol_event_in_logs() {
  local event="$1"
  local deadline="$2"
  local qemu_pid="$3"
  local tail_lines="$4"
  shift 4

  local regex
  regex="$(aicp_protocol_event_regex "${event}")" || return
  local log_files=("$@")

  while ((SECONDS < deadline)); do
    if aicp_logs_match_regex "${regex}" "${log_files[@]}"; then
      echo "[ai-rtos] observed AICP protocol event: ${event}"
      return 0
    fi
    if ! kill -0 "${qemu_pid}" 2>/dev/null; then
      echo "[ai-rtos] AxVisor exited before AICP protocol event: ${event}" >&2
      aicp_print_log_tails "${tail_lines}" "${log_files[@]}"
      return 1
    fi
    sleep 1
  done

  echo "[ai-rtos] timeout waiting for AICP protocol event: ${event}" >&2
  aicp_print_log_tails "${tail_lines}" "${log_files[@]}"
  return 1
}

aicp_wait_for_marker() {
  local marker="$1"
  local deadline="$2"
  local qemu_pid="$3"
  local log_file="$4"
  local tail_lines="$5"

  aicp_wait_for_marker_in_logs \
    "${marker}" "${deadline}" "${qemu_pid}" "${tail_lines}" "${log_file}"
}

aicp_wait_for_any_marker() {
  local description="$1"
  local deadline="$2"
  local qemu_pid="$3"
  local log_file="$4"
  local tail_lines="$5"
  shift 5

  aicp_wait_for_any_marker_in_logs \
    "${description}" "${deadline}" "${qemu_pid}" "${tail_lines}" \
    1 "${log_file}" "$@"
}

aicp_wait_for_arceos_ready_in_logs() {
  local deadline="$1"
  local qemu_pid="$2"
  local tail_lines="$3"
  local log_count="$4"
  shift 4

  aicp_wait_for_any_marker_in_logs \
    "ArceOS AICP server ready" "${deadline}" "${qemu_pid}" "${tail_lines}" \
    "${log_count}" "$@" \
    "AICP_RTOS_READY" \
    "AICP ArceOS RTOS TCP server listening" \
    "AICP ArceOS RTOS UDP server listening" \
    "AICP ArceOS RTOS server listening" \
    "AICP client connected:"
}

aicp_wait_for_arceos_ready() {
  local deadline="$1"
  local qemu_pid="$2"
  local log_file="$3"
  local tail_lines="$4"

  aicp_wait_for_arceos_ready_in_logs \
    "${deadline}" "${qemu_pid}" "${tail_lines}" 1 "${log_file}"
}
