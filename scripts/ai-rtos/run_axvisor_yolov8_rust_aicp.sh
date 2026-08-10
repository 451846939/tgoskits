#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
#
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  scripts/ai-rtos/run_axvisor_yolov8_rust_aicp.sh [boot_timeout_seconds]

Build and boot two AxVisor guests:
  Linux Rust YOLOv8 guest  10.0.3.3/24, 2 vCPUs on pCPU0,pCPU1
  ArceOS RTOS guest         10.0.3.2/24, 1 vCPU on pCPU2

The Linux /init executes real YOLOv8n ONNX Runtime CPU inference in a Rust
application and sends AICP CONTROL_SET frames to the RTOS guest over TCP/IP.

Set AICP_YOLO_RUST_SKIP_BUILD=1 to reuse install/aarch64.
EOF
}

if [[ $# -gt 1 || "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage >&2
  exit 2
fi

boot_timeout_s="${1:-360}"
repo_root="${AICP_TGOSKITS_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)}"
source "${repo_root}/scripts/ai-rtos/lib/dtb.sh"
source "${repo_root}/scripts/ai-rtos/lib/host_tools.sh"
source "${repo_root}/scripts/ai-rtos/lib/markers.sh"
source "${repo_root}/scripts/ai-rtos/lib/process.sh"
demo_dir="${AICP_RUST_YOLO_DEMO_DIR:-${repo_root}/apps/ai-rtos-demo/yolov8-rust-onnx}"
install_dir="${demo_dir}/install/aarch64"
out_dir="${repo_root}/tmp/ai-rtos"
log_dir="${out_dir}/logs"
bundle_dir="${repo_root}/tmp/images/qemu-aarch64"
stamp="$(date +%Y%m%d-%H%M%S)"
log_file="${log_dir}/axvisor-yolov8-rust-aicp-${stamp}.log"
linux_src_dts="${repo_root}/os/axvisor/configs/vms/qemu/aarch64/linux-smp1.dts"
linux_dts="${out_dir}/linux-yolov8-rust.dts"
linux_dtb="${out_dir}/linux-yolov8-rust.dtb"
linux_vm="${out_dir}/linux-yolov8-rust.generated.toml"
rtos_vm="${out_dir}/arceos-yolov8-rust.generated.toml"
initramfs_dir="${out_dir}/yolov8-rust-initramfs"
initramfs="${out_dir}/linux-yolov8-rust-initramfs.cpio.gz"
linux_kernel="${bundle_dir}/linux/linux-qemu"
arceos_bin="${repo_root}/target/aarch64-unknown-linux-musl/release/arceos-aicp-server.bin"
qemu_pid=""

mkdir -p "${out_dir}" "${log_dir}"

cleanup() {
  aicp_cleanup_process_tree "${qemu_pid}"
}
trap cleanup EXIT

wait_for_marker() {
  aicp_wait_for_marker "$1" "$((SECONDS + boot_timeout_s))" \
    "${qemu_pid}" "${log_file}" 200
}

wait_for_any_marker() {
  local description="$1"
  shift
  aicp_wait_for_any_marker "${description}" "$((SECONDS + boot_timeout_s))" \
    "${qemu_pid}" "${log_file}" 200 "$@"
}

write_linux_vm_config() {
  cat >"${linux_vm}" <<EOF
[base]
id = 1
name = "linux-yolov8-rust-qemu"
vm_type = 1
cpu_num = 2
phys_cpu_ids = [0, 1]

[kernel]
entry_point = 0x8020_0000
image_location = "memory"
kernel_path = "${linux_kernel}"
kernel_load_addr = 0x8020_0000
dtb_path = "${linux_dtb}"
dtb_load_addr = 0x8000_0000
ramdisk_path = "${initramfs}"
ramdisk_load_addr = 0x9000_0000
memory_regions = [
  [0x8000_0000, 0x4000_0000, 0x7, 1],
]

[devices]
interrupt_mode = "passthrough"
passthrough_devices = [
  ["/soc/virtio_mmio@a003c00"],
]
passthrough_addresses = []
excluded_devices = []
emu_devices = []
EOF
}

write_rtos_vm_config() {
  cat >"${rtos_vm}" <<EOF
[base]
id = 2
name = "arceos-aicp-yolov8-rust-qemu"
vm_type = 1
cpu_num = 1
phys_cpu_ids = [2]

[kernel]
entry_point = 0x8020_0000
image_location = "memory"
kernel_path = "${arceos_bin}"
kernel_load_addr = 0x8020_0000
dtb_load_addr = 0x8000_0000
memory_regions = [
  [0x8000_0000, 0x4000_0000, 0x7, 1],
]

[devices]
interrupt_mode = "passthrough"
passthrough_devices = [
  ["/"],
]
passthrough_addresses = []
excluded_devices = [
  ["/pcie@10000000"],
  ["/soc/virtio_mmio@a003c00"],
]
emu_devices = []
EOF
}

if [[ ! -f "${linux_kernel}" ]]; then
  echo "ERROR: Linux kernel not found: ${linux_kernel}" >&2
  echo "Run: cargo xtask image pull qemu-aarch64 -o tmp/images" >&2
  exit 1
fi

if [[ "${AICP_YOLO_RUST_SKIP_BUILD:-0}" != "1" ]] &&
   { [[ "${AICP_YOLO_RUST_REBUILD:-0}" == "1" ]] ||
     ! aicp_yolo_rust_bundle_ready "${install_dir}"; }; then
  echo "[ai-rtos] Building Rust YOLOv8 ONNX CPU aarch64 package..."
  AICP_TGOSKITS_ROOT="${repo_root}" "${demo_dir}/build-aarch64-docker.sh"
elif aicp_yolo_rust_bundle_ready "${install_dir}"; then
  echo "[ai-rtos] Reusing complete Rust YOLOv8 aarch64 runtime bundle"
fi
if ! aicp_yolo_rust_bundle_ready "${install_dir}"; then
  echo "ERROR: incomplete Rust YOLOv8 runtime bundle: ${install_dir}" >&2
  exit 1
fi

echo "[ai-rtos] Packing Rust YOLOv8 initramfs: ${initramfs}"
rm -rf "${initramfs_dir}"
mkdir -p "${initramfs_dir}"
cp "${install_dir}/aicp_yolov8_rust_onnx" "${initramfs_dir}/init"
chmod +x "${initramfs_dir}/init"
cp -a "${install_dir}/lib" "${initramfs_dir}/lib"
cp -a "${install_dir}/model" "${initramfs_dir}/model"
cp -a "${install_dir}/validation" "${initramfs_dir}/validation"
(
  cd "${initramfs_dir}"
  find . -print | cpio -o -H newc | gzip -9 >"${initramfs}"
)

echo "[ai-rtos] Building ArceOS RTOS guest with 10.0.3.2/24..."
(
  cd "${repo_root}"
  AX_IP=10.0.3.2 AX_GW=0.0.0.0 AX_PREFIX_LEN=24 \
    cargo xtask arceos build \
      -p arceos-aicp-server \
      --arch aarch64 \
      --config apps/arceos/build-aarch64-unknown-none-softfloat.toml
)
if [[ ! -f "${arceos_bin}" ]]; then
  echo "ERROR: ArceOS AICP image not found: ${arceos_bin}" >&2
  exit 1
fi

crop_virtio_nodes "${linux_src_dts}" "${linux_dts}" "virtio_mmio@a003c00"
patch_bootargs "${linux_dts}" "console=ttyAMA0 earlycon=pl011,0x9000000 rdinit=/init panic=-1 loglevel=7"
dtc -I dts -O dtb -o "${linux_dtb}" "${linux_dts}"
write_linux_vm_config
write_rtos_vm_config

echo "[ai-rtos] Booting AxVisor Rust YOLOv8 closed loop; log: ${log_file}"
(
  cd "${repo_root}"
  cargo xtask axvisor qemu \
    --config os/axvisor/configs/board/qemu-aarch64.toml \
    --qemu-config os/axvisor/configs/qemu/qemu-aarch64-aicp-dual-net.toml \
    --vmconfigs "${rtos_vm}" \
    --vmconfigs "${linux_vm}"
) >"${log_file}" 2>&1 &
qemu_pid=$!

if ! wait_for_any_marker "Linux or RTOS guest startup" \
  "AICP_YOLO_RUST_BEGIN" \
  "AICP_RTOS_READY" \
  "AICP ArceOS RTOS TCP server listening" \
  "AICP client connected:"; then
  echo "[ai-rtos] neither guest emitted a startup marker" >&2
  exit 1
fi
wait_for_marker "AICP_YOLO_RUST_BEGIN"
wait_for_marker "AICP_YOLO_RUST_DONE ok="

if ! grep -q "AICP_YOLO_RUST_DONE ok=.*failed=0" "${log_file}"; then
  echo "[ai-rtos] FAIL: Rust YOLOv8 guest reported failures" >&2
  tail -n 200 "${log_file}" >&2 || true
  exit 1
fi
if ! grep -q "AICP_YOLO_RUST_RESULT" "${log_file}"; then
  echo "[ai-rtos] FAIL: no Rust YOLOv8 inference result" >&2
  tail -n 200 "${log_file}" >&2 || true
  exit 1
fi
if ! grep -q "AICP_YOLO_RUST_CONTROL" "${log_file}"; then
  echo "[ai-rtos] FAIL: no Rust AICP STATUS response" >&2
  tail -n 200 "${log_file}" >&2 || true
  exit 1
fi
if ! grep -q "CONTROL seq=" "${log_file}"; then
  echo "[ai-rtos] FAIL: RTOS guest did not apply CONTROL_SET" >&2
  tail -n 200 "${log_file}" >&2 || true
  exit 1
fi

grep "AICP_YOLO_RUST_DONE" "${log_file}" | tail -n 1
echo "[ai-rtos] PASS: AxVisor Rust YOLOv8 ONNX CPU AICP closed loop complete"
echo "[ai-rtos] log: ${log_file}"
