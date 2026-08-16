#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
#
# SPDX-License-Identifier: Apache-2.0

aicp_build_arceos_guest() {
  local repo_root="$1"
  local build_config="$2"
  local elf="${repo_root}/target/aarch64-unknown-linux-musl/release/arceos-aicp-server"
  local image="${elf}.bin"
  local objcopy=""

  (
    cd "${repo_root}"
    cargo xtask arceos build \
      -p arceos-aicp-server \
      --arch aarch64 \
      --config "${build_config}"
  )

  if [[ ! -s "${elf}" ]]; then
    echo "ERROR: ArceOS AICP ELF is missing or empty: ${elf}" >&2
    return 1
  fi

  for candidate in rust-objcopy llvm-objcopy aarch64-linux-musl-objcopy; do
    if command -v "${candidate}" >/dev/null 2>&1; then
      objcopy="${candidate}"
      break
    fi
  done
  if [[ -z "${objcopy}" ]]; then
    echo "ERROR: rust-objcopy, llvm-objcopy, or aarch64-linux-musl-objcopy is required" >&2
    return 1
  fi

  "${objcopy}" --strip-all -O binary "${elf}" "${image}"
  if [[ ! -s "${image}" ]]; then
    echo "ERROR: ArceOS AICP raw image is missing or empty: ${image}" >&2
    return 1
  fi
}
