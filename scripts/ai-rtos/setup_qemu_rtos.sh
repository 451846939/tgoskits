#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
#
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  scripts/ai-rtos/setup_qemu_rtos.sh <zephyr|arceos-aicp|all>

Prepares the QEMU AArch64 image bundle entries and generated AxVisor VM
configs that can be consumed directly from the TGOSKits image archive.
For Zephyr the script uses the current TGOSKits image command:

  cargo xtask image pull qemu-aarch64 -o tmp/images

The AICP FreeRTOS guest is built from the repository-owned application and
requires a dedicated reserved-memory host DTB. Use:

  scripts/ai-rtos/build_freertos_aicp_guest.sh
  scripts/ai-rtos/run_freertos_aicp_guest_smoke.sh

Outputs:
  tmp/ai-rtos/zephyr-qemu.generated.toml
  tmp/ai-rtos/arceos-aicp-qemu.generated.toml
EOF
}

if [[ $# -ne 1 ]]; then
  usage >&2
  exit 2
fi

guest="$1"
case "${guest}" in
  zephyr|arceos-aicp|all) ;;
  *)
    usage >&2
    exit 2
    ;;
esac

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
out_dir="${repo_root}/tmp/ai-rtos"
bundle_dir="${repo_root}/tmp/images/qemu-aarch64"

mkdir -p "${out_dir}" "${repo_root}/tmp/images"

if [[ "${guest}" == "zephyr" || "${guest}" == "all" ]]; then
  echo "[ai-rtos] Preparing qemu-aarch64 guest image bundle..."
  (
    cd "${repo_root}"
    cargo xtask image pull qemu-aarch64 -o tmp/images
  )
fi

patch_config() {
  local src="$1"
  local dst="$2"
  local image="$3"
  local dts="$4"
  local dtb="$5"
  local entry_point="${6:-}"

  cp "${src}" "${dst}"
  dtc -I dts -O dtb -o "${dtb}" "${dts}"
  sed -i.bak \
    -e 's|^image_location *=.*|image_location = "memory"|' \
    -e 's|^kernel_path *=.*|kernel_path = "'"${image}"'"|' \
    "${dst}"
  if [[ -n "${entry_point}" ]]; then
    sed -i.bak \
      -e 's|^entry_point *=.*|entry_point = '"${entry_point}"'|' \
      "${dst}"
  fi
  if grep -q '^# *dtb_path *=' "${dst}"; then
    sed -i.bak -e 's|^# *dtb_path *=.*|dtb_path = "'"${dtb}"'"|' "${dst}"
  elif grep -q '^dtb_path *=' "${dst}"; then
    sed -i.bak -e 's|^dtb_path *=.*|dtb_path = "'"${dtb}"'"|' "${dst}"
  else
    sed -i.bak -e '/^dtb_load_addr *=/i\
dtb_path = "'"${dtb}"'"
' "${dst}"
  fi
  rm -f "${dst}.bak"
  echo "[ai-rtos] Generated ${dst}"
}

read_elf_entry() {
  local elf="$1"

  python3 - "${elf}" <<'PY'
import struct
import sys

path = sys.argv[1]
with open(path, "rb") as stream:
    header = stream.read(64)

if len(header) < 64 or header[:4] != b"\x7fELF":
    raise SystemExit(f"ERROR: not an ELF file: {path}")

elf_class = header[4]
endianness = header[5]
if elf_class != 2:
    raise SystemExit(f"ERROR: expected ELF64 image: {path}")
if endianness == 1:
    byte_order = "<"
elif endianness == 2:
    byte_order = ">"
else:
    raise SystemExit(f"ERROR: unsupported ELF byte order: {path}")

entry = struct.unpack_from(f"{byte_order}Q", header, 24)[0]
print(f"0x{entry:x}")
PY
}

patch_arceos_aicp_config() {
  local src="$1"
  local dst="$2"
  local image="$3"

  cp "${src}" "${dst}"
  sed -i.bak \
    -e 's|^id *=.*|id = 2|' \
    -e 's|^name *=.*|name = "arceos-aicp-qemu"|' \
    -e 's|^phys_cpu_ids *=.*|phys_cpu_ids = [2]|' \
    -e 's|^image_location *=.*|image_location = "memory"|' \
    -e 's|^kernel_path *=.*|kernel_path = "'"${image}"'"|' \
    "${dst}"
  perl -0pi -e 's|memory_regions = \[\n  \[0x8000_0000, 0x4000_0000, 0x7, 1\], # System RAM 1G MAP_IDENTICAL\n\]|memory_regions = [\n  [0x8000_0000, 0x4000_0000, 0x7, 1], # System RAM 1G MAP_IDENTICAL\n]|' "${dst}"
  rm -f "${dst}.bak"
  echo "[ai-rtos] Generated ${dst}"
}

if [[ "${guest}" == "zephyr" || "${guest}" == "all" ]]; then
  zephyr_image="${bundle_dir}/zephyr/zephyr-qemu"
  zephyr_elf="${bundle_dir}/zephyr/zephyr-qemu.elf"
  if [[ ! -f "${zephyr_image}" ]]; then
    echo "ERROR: Zephyr image not found at ${zephyr_image}" >&2
    exit 1
  fi
  if [[ ! -f "${zephyr_elf}" ]]; then
    echo "ERROR: Zephyr ELF not found at ${zephyr_elf}" >&2
    exit 1
  fi
  zephyr_entry="$(read_elf_entry "${zephyr_elf}")"
  echo "[ai-rtos] Zephyr ELF entry point: ${zephyr_entry}"
  patch_config \
    "${repo_root}/os/axvisor/configs/vms/qemu/aarch64/zephyr-smp1.toml" \
    "${out_dir}/zephyr-qemu.generated.toml" \
    "${zephyr_image}" \
    "${repo_root}/os/axvisor/configs/vms/qemu/aarch64/zephyr-smp1.dts" \
    "${out_dir}/zephyr-smp1.dtb" \
    "${zephyr_entry}"
fi

if [[ "${guest}" == "arceos-aicp" || "${guest}" == "all" ]]; then
  arceos_bin="${repo_root}/target/aarch64-unknown-linux-musl/release/arceos-aicp-server.bin"
  echo "[ai-rtos] Building ArceOS AICP RTOS guest..."
  (
    cd "${repo_root}"
    cargo xtask arceos build \
      -p arceos-aicp-server \
      --arch aarch64 \
      --config apps/arceos/build-aarch64-unknown-none-softfloat.toml
  )
  if [[ ! -f "${arceos_bin}" ]]; then
    echo "ERROR: ArceOS AICP image not found at ${arceos_bin}" >&2
    exit 1
  fi
  patch_arceos_aicp_config \
    "${repo_root}/os/axvisor/configs/vms/qemu/aarch64/arceos-smp1.toml" \
    "${out_dir}/arceos-aicp-qemu.generated.toml" \
    "${arceos_bin}"
fi

cat <<EOF

[ai-rtos] Done.
Run examples:
  cargo xtask axvisor qemu \\
    --config os/axvisor/configs/board/qemu-aarch64.toml \\
    --qemu-config os/axvisor/configs/qemu/qemu-aarch64.toml \\
    --vmconfigs tmp/ai-rtos/zephyr-qemu.generated.toml

  cargo xtask axvisor qemu \\
    --config os/axvisor/configs/board/qemu-aarch64.toml \\
    --qemu-config os/axvisor/configs/qemu/qemu-aarch64.toml \\
    --vmconfigs tmp/ai-rtos/arceos-aicp-qemu.generated.toml
EOF
