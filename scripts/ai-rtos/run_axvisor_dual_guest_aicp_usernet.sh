#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
#
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  scripts/ai-rtos/run_axvisor_dual_guest_aicp_usernet.sh [iterations] [ai|fixed] [boot_timeout_seconds]

Boots two AxVisor guests in one QEMU AArch64 run:
  Linux AI guest     10.0.2.15/24 via QEMU user-net, 2 vCPUs on pCPU0,pCPU1
  ArceOS RTOS guest  10.0.2.15/24 behind a separate QEMU user-net, 1 vCPU on pCPU2

The Linux guest connects to 10.0.2.2:18800. QEMU forwards that TCP/IP flow to
the RTOS guest's 10.0.2.15:8800 listener. The application payload is the AICP
protocol over TCP; vsock/shared-memory/hypercall paths are not used.
EOF
}

if [[ $# -gt 3 || "${1:-}" == "-h" ]]; then
  usage >&2
  exit 2
fi

iterations="${1:-40}"
mode="${2:-ai}"
boot_timeout_s="${3:-150}"

if [[ "${mode}" != "ai" && "${mode}" != "fixed" ]]; then
  usage >&2
  exit 2
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
source "${repo_root}/scripts/ai-rtos/lib/dtb.sh"
source "${repo_root}/scripts/ai-rtos/lib/markers.sh"
source "${repo_root}/scripts/ai-rtos/lib/process.sh"
demo_dir="${repo_root}/apps/ai-rtos-demo"
out_dir="${repo_root}/tmp/ai-rtos"
log_dir="${out_dir}/logs"
bundle_dir="${repo_root}/tmp/images/qemu-aarch64"
mkdir -p "${out_dir}" "${log_dir}" "${demo_dir}/build/aarch64"
host_port="${AICP_HOST_PORT:-18800}"
stress_procs="${AICP_STRESS_PROCS:-0}"

if ! [[ "${host_port}" =~ ^[0-9]+$ ]] || (( host_port < 1024 || host_port > 65535 )); then
  echo "ERROR: AICP_HOST_PORT must be a TCP port in [1024, 65535], got '${host_port}'" >&2
  exit 2
fi
if ! [[ "${stress_procs}" =~ ^[0-9]+$ ]] || (( stress_procs > 16 )); then
  echo "ERROR: AICP_STRESS_PROCS must be an integer in [0, 16], got '${stress_procs}'" >&2
  exit 2
fi

stamp="$(date +%Y%m%d-%H%M%S)"
log_file="${log_dir}/axvisor-dual-guest-aicp-usernet-${stamp}.log"
qemu_config_template="${repo_root}/os/axvisor/configs/qemu/qemu-aarch64-aicp-dual-usernet.toml"
qemu_config="${out_dir}/qemu-aarch64-aicp-dual-usernet-${host_port}.toml"
linux_src_dts="${repo_root}/os/axvisor/configs/vms/qemu/aarch64/linux-smp1.dts"
linux_dts="${out_dir}/linux-aicp-usernet.dts"
linux_dtb="${out_dir}/linux-aicp-usernet.dtb"
rtos_dts="${out_dir}/arceos-aicp-usernet.dts"
rtos_dtb="${out_dir}/arceos-aicp-usernet.dtb"
linux_vm="${out_dir}/linux-aicp-usernet.generated.toml"
rtos_vm="${out_dir}/arceos-aicp-usernet.generated.toml"
initramfs_dir="${out_dir}/initramfs-usernet"
initramfs="${out_dir}/linux-aicp-usernet-initramfs.cpio.gz"
linux_kernel="${bundle_dir}/linux/linux-qemu"
arceos_bin="${repo_root}/target/aarch64-unknown-linux-musl/release/arceos-aicp-server.bin"

cleanup() {
  aicp_cleanup_process_tree "${qemu_pid:-}"
}
trap cleanup EXIT

wait_for_marker() {
  aicp_wait_for_marker "$1" "$((SECONDS + boot_timeout_s))" \
    "${qemu_pid}" "${log_file}" 160
}

wait_for_any_marker() {
  local description="$1"
  shift
  aicp_wait_for_any_marker "${description}" "$((SECONDS + boot_timeout_s))" \
    "${qemu_pid}" "${log_file}" 160 "$@"
}

write_qemu_config() {
  HOST_PORT="${host_port}" perl -0pe \
    's#hostfwd=tcp::[0-9]+-:8800#hostfwd=tcp::$ENV{HOST_PORT}-:8800#g' \
    "${qemu_config_template}" > "${qemu_config}"
}

write_linux_vm_config() {
  cat > "${linux_vm}" <<EOF
[base]
id = 1
name = "linux-ai-usernet-qemu"
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
  [0x8000_0000, 0x2000_0000, 0x7, 1],
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
  cat > "${rtos_vm}" <<EOF
[base]
id = 2
name = "arceos-aicp-usernet-qemu"
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
  echo "ERROR: Linux kernel not found at ${linux_kernel}" >&2
  echo "Run: cargo xtask image pull qemu-aarch64 --extract-dir tmp/images" >&2
  exit 1
fi

echo "[ai-rtos] Building static Linux guest /init for user-net hostfwd..."
rm -f "${demo_dir}/build/aarch64/aicp_init"
make -C "${demo_dir}" linux-init-aarch64 \
  CFLAGS="-O2 -g -Wall -Wextra -Werror -std=c11 -DAICP_INIT_ITERATIONS=${iterations}u -DAICP_INIT_MODE=\\\"${mode}\\\" -DAICP_INIT_SERVER=\\\"10.0.2.2\\\" -DAICP_INIT_SERVER_PORT=${host_port}u -DAICP_INIT_CLIENT=\\\"10.0.2.15\\\" -DAICP_INIT_NETMASK=\\\"255.255.255.0\\\" -DAICP_INIT_STATIC_ARP=0 -DAICP_INIT_STRESS_PROCS=${stress_procs}u"

echo "[ai-rtos] Packing initramfs: ${initramfs}"
rm -rf "${initramfs_dir}"
mkdir -p "${initramfs_dir}"
cp "${demo_dir}/build/aarch64/aicp_init" "${initramfs_dir}/init"
chmod +x "${initramfs_dir}/init"
(
  cd "${initramfs_dir}"
  find . -print | cpio -o -H newc | gzip -9 > "${initramfs}"
)

echo "[ai-rtos] Building ArceOS RTOS guest with DHCP on QEMU user-net..."
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

echo "[ai-rtos] Generating guest DTBs..."
crop_virtio_nodes "${linux_src_dts}" "${rtos_dts}" "virtio_mmio@a003c00"
crop_virtio_nodes "${linux_src_dts}" "${linux_dts}" "virtio_mmio@a003c00"
patch_bootargs "${linux_dts}" "console=ttyAMA0 earlycon=pl011,0x9000000 rdinit=/init panic=-1 loglevel=7"
dtc -I dts -O dtb -o "${rtos_dtb}" "${rtos_dts}"
dtc -I dts -O dtb -o "${linux_dtb}" "${linux_dts}"

write_linux_vm_config
write_rtos_vm_config
write_qemu_config

echo "[ai-rtos] Booting dual guest AxVisor user-net host_port=${host_port} stress_procs=${stress_procs}; log: ${log_file}"
(
  cd "${repo_root}"
  cargo xtask axvisor qemu \
    --config os/axvisor/configs/board/qemu-aarch64.toml \
    --qemu-config "${qemu_config}" \
    --vmconfigs "${rtos_vm}" \
    --vmconfigs "${linux_vm}"
) >"${log_file}" 2>&1 &
qemu_pid=$!

wait_for_any_marker "RTOS AICP server ready" \
  "AICP_RTOS_READY" \
  "AICP ArceOS RTOS server listening" \
  "AICP client connected:"
wait_for_marker "AICP Linux guest client starting"
wait_for_any_marker "Linux AICP TCP connection" \
  "AICP Linux guest connected" \
  "AICP client connected:" \
  "AICP HELLO"
wait_for_marker "AICP_LINUX_DONE ok="

if ! grep -q "AICP_LINUX_DONE ok=.*failed=0" "${log_file}"; then
  echo "[ai-rtos] FAIL: Linux guest AICP client reported failures" >&2
  tail -n 160 "${log_file}" >&2 || true
  exit 1
fi
if ! grep -q "CONTROL seq=" "${log_file}"; then
  echo "[ai-rtos] FAIL: RTOS guest did not log CONTROL messages" >&2
  tail -n 160 "${log_file}" >&2 || true
  exit 1
fi

grep "AICP_LINUX_DONE" "${log_file}" | tail -n 1
echo "[ai-rtos] PASS: dual guest AxVisor AICP TCP/IP user-net closed loop complete"
echo "[ai-rtos] log: ${log_file}"
