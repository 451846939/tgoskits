#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
#
# SPDX-License-Identifier: Apache-2.0

patch_linux_guest_console() {
  local path="$1"
  perl -0pi -e \
    's/pl011\@9000000/pl011\@9040000/g; s/0x9000000/0x9040000/g; s/interrupts = <0x00 0x01 0x04>;/interrupts = <0x00 0x08 0x04>;/' \
    "${path}"
}

crop_virtio_nodes() {
  local src="$1"
  local dst="$2"
  local keep="$3"
  awk -v keep="${keep}" '
    /^[[:space:]]*virtio_mmio@/ {
      if ($0 !~ keep) {
        skip = 1
        depth = 0
      }
    }
    skip {
      depth += gsub(/\{/, "{")
      depth -= gsub(/\}/, "}")
      if (depth <= 0 && /};/) skip = 0
      next
    }
    { print }
  ' "${src}" > "${dst}"
}

remove_dts_nodes() {
  local src="$1"
  local dst="$2"
  local pattern="$3"
  awk -v pattern="${pattern}" '
    /^[[:space:]]*[A-Za-z0-9,._+*-]+@?[A-Fa-f0-9]*[[:space:]]*\{/ {
      if ($0 ~ pattern) {
        skip = 1
        depth = 0
      }
    }
    skip {
      depth += gsub(/\{/, "{")
      depth -= gsub(/\}/, "}")
      if (depth <= 0 && /};/) skip = 0
      next
    }
    { print }
  ' "${src}" > "${dst}"
}

patch_bootargs() {
  local path="$1"
  local bootargs="$2"
  BOOTARGS="${bootargs}" perl -0pi -e \
    's/bootargs = "[^"]*";/bootargs = "$ENV{BOOTARGS}";/' "${path}"
}
