#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
#
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  scripts/ai-rtos/run_axvisor_dual_guest_aicp.sh [iterations] [ai|fixed] [boot_timeout_seconds]

Boots two AxVisor guests in one QEMU AArch64 run:
  Linux AI guest    10.0.3.3/24, 2 vCPUs pinned to pCPU2,pCPU3
  ArceOS RTOS guest 10.0.3.2/24, 1 vCPU pinned to pCPU1

The default four-core layout reserves pCPU0 for AxVisor housekeeping. Override
the topology with AICP_HOST_CPUS, AICP_LINUX_VCPU0_PCPU,
AICP_LINUX_VCPU1_PCPU, and AICP_RTOS_VCPU0_PCPU.

Set AICP_CLIENT_IMPL=c or AICP_CLIENT_IMPL=rust to select the Linux client.
Both guests communicate through an isolated QEMU layer-2 hub.
EOF
}

if [[ $# -gt 3 || "${1:-}" == "-h" ]]; then
  usage >&2
  exit 2
fi

iterations="${1:-40}"
mode="${2:-ai}"
boot_timeout_s="${3:-180}"
client_impl="${AICP_CLIENT_IMPL:-c}"

if [[ "${mode}" != "ai" && "${mode}" != "fixed" ]]; then
  usage >&2
  exit 2
fi
if [[ "${client_impl}" != "c" && "${client_impl}" != "rust" ]]; then
  echo "ERROR: AICP_CLIENT_IMPL must be c or rust, got '${client_impl}'" >&2
  exit 2
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
source "${repo_root}/scripts/ai-rtos/lib/cpu_topology.sh"
source "${repo_root}/scripts/ai-rtos/lib/dtb.sh"
source "${repo_root}/scripts/ai-rtos/lib/markers.sh"
source "${repo_root}/scripts/ai-rtos/lib/process.sh"

demo_dir="${repo_root}/apps/ai-rtos-demo"
out_dir="${repo_root}/tmp/ai-rtos"
log_dir="${out_dir}/logs"
bundle_dir="${repo_root}/tmp/images/qemu-aarch64"
mkdir -p "${out_dir}" "${log_dir}" "${demo_dir}/build/aarch64"

stress_procs="${AICP_STRESS_PROCS:-0}"
axvisor_board_config="${AICP_AXVISOR_BOARD_CONFIG:-os/axvisor/configs/board/qemu-aarch64.toml}"
if ! [[ "${stress_procs}" =~ ^[0-9]+$ ]] || (( stress_procs > 16 )); then
  echo "ERROR: AICP_STRESS_PROCS must be an integer in [0, 16], got '${stress_procs}'" >&2
  exit 2
fi
aicp_configure_dual_guest_cpu_topology

stamp="$(date +%Y%m%d-%H%M%S)"
run_name="axvisor-dual-guest-aicp-${client_impl}"
log_file="${log_dir}/${run_name}-${stamp}.log"
linux_console_log="${log_dir}/${run_name}-linux-console-${stamp}.log"
linux_src_dts="${repo_root}/os/axvisor/configs/vms/qemu/aarch64/linux-smp1.dts"
linux_dts="${out_dir}/${run_name}-linux.dts"
linux_dtb="${out_dir}/${run_name}-linux.dtb"
rtos_dts="${out_dir}/${run_name}-arceos.dts"
rtos_dtb="${out_dir}/${run_name}-arceos.dtb"
linux_vm="${out_dir}/${run_name}-linux.generated.toml"
rtos_vm="${out_dir}/${run_name}-arceos.generated.toml"
qemu_config="${out_dir}/${run_name}-qemu.generated.toml"
qemu_template="${repo_root}/os/axvisor/configs/qemu/qemu-aarch64-aicp-dual-net.toml"
host_base_dtb="${out_dir}/${run_name}-host-base.dtb"
host_overlay_dts="${repo_root}/configs/ai-rtos/qemu-aarch64-arceos-reserved-memory-overlay.dts"
host_overlay_dtbo="${out_dir}/${run_name}-host-overlay.dtbo"
host_dtb="${out_dir}/${run_name}-host.dtb"
host_dtb_dummy_disk="${out_dir}/${run_name}-dummy-disk.img"
initramfs_dir="${out_dir}/${run_name}-initramfs"
initramfs="${out_dir}/${run_name}-initramfs.cpio.gz"
linux_kernel="${bundle_dir}/linux/linux-qemu"
arceos_bin="${repo_root}/target/aarch64-unknown-linux-musl/release/arceos-aicp-server.bin"

cleanup() {
  aicp_cleanup_process_tree "${qemu_pid:-}"
}
trap cleanup EXIT

wait_for_marker() {
  aicp_wait_for_marker_in_logs "$1" "$((SECONDS + boot_timeout_s))" \
    "${qemu_pid}" 180 "${log_file}" "${linux_console_log}"
}

wait_for_any_marker() {
  local description="$1"
  shift
  aicp_wait_for_any_marker_in_logs \
    "${description}" "$((SECONDS + boot_timeout_s))" "${qemu_pid}" 180 \
    2 "${log_file}" "${linux_console_log}" "$@"
}

require_tool() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "ERROR: missing required tool: $1" >&2
    exit 1
  fi
}

build_linux_initramfs() {
  rm -rf "${initramfs_dir}"
  mkdir -p "${initramfs_dir}"

  if [[ "${client_impl}" == "rust" ]]; then
    local rust_init="${AICP_RUST_CLIENT_BIN:-${demo_dir}/build/aarch64/aicp_rust_client}"
    if [[ -z "${AICP_RUST_CLIENT_BIN:-}" ]]; then
      make -C "${demo_dir}" rust-client-aarch64
    fi
    if [[ ! -s "${rust_init}" ]]; then
      echo "ERROR: Rust Linux guest /init is missing or empty: ${rust_init}" >&2
      exit 1
    fi
    cp "${rust_init}" "${initramfs_dir}/init"
  else
    rm -f "${demo_dir}/build/aarch64/aicp_init"
    make -C "${demo_dir}" linux-init-aarch64 \
      CFLAGS="-O2 -g -Wall -Wextra -Werror -std=c11 -DAICP_INIT_ITERATIONS=${iterations}u -DAICP_INIT_MODE=\\\"${mode}\\\" -DAICP_INIT_STRESS_PROCS=${stress_procs}u"
    cp "${demo_dir}/build/aarch64/aicp_init" "${initramfs_dir}/init"
  fi

  chmod +x "${initramfs_dir}/init"
  (
    cd "${initramfs_dir}"
    find . -print | cpio -o -H newc | gzip -9 > "${initramfs}"
  )
}

build_host_dtb() {
  : > "${host_dtb_dummy_disk}"
  qemu-system-aarch64 \
    -display none \
    -monitor none \
    -serial null \
    -serial "file:${linux_console_log}" \
    -cpu cortex-a72 \
    -machine "virt,virtualization=on,gic-version=3,dumpdtb=${host_base_dtb}" \
    -smp "${host_cpus}" \
    -m 8g \
    -device virtio-blk-device,drive=disk0 \
    -drive "id=disk0,if=none,format=raw,file=${host_dtb_dummy_disk}" \
    -netdev hubport,id=linuxnet,hubid=3 \
    -device virtio-net-device,netdev=linuxnet,mac=52:54:00:aa:03:03 \
    -netdev hubport,id=rtosnet,hubid=3 \
    -device virtio-net-device,netdev=rtosnet,mac=52:54:00:aa:03:02

  dtc -@ -I dts -O dtb -o "${host_overlay_dtbo}" "${host_overlay_dts}"
  fdtoverlay -i "${host_base_dtb}" -o "${host_dtb}" "${host_overlay_dtbo}"

  local reserved_reg
  reserved_reg="$(fdtget -tx "${host_dtb}" /reserved-memory/arceos@c0000000 reg)"
  if [[ "${reserved_reg}" != "0 c0000000 0 10000000" ]]; then
    echo "ERROR: invalid ArceOS reserved-memory region: ${reserved_reg}" >&2
    exit 1
  fi
}

write_linux_vm_config() {
  cat > "${linux_vm}" <<EOF
[base]
id = 1
name = "linux-ai-dual-qemu"
vm_type = 1
cpu_num = 2
phys_cpu_ids = [${linux_vcpu0_pcpu}, ${linux_vcpu1_pcpu}]
phys_cpu_sets = [${linux_vcpu0_mask}, ${linux_vcpu1_mask}]

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
  ["/virtio_mmio@a003c00"],
  ["/pl011@9040000"],
]
passthrough_addresses = []
excluded_devices = []
emu_devices = [
  ["gppt-gicd", 0x0800_0000, 0x1_0000, 0, 0x21, []],
  ["gppt-gicr", 0x080a_0000, 0x2_0000, 0, 0x20, [2, 0x2_0000, ${linux_vcpu0_pcpu}]],
]
EOF
}

write_rtos_vm_config() {
  cat > "${rtos_vm}" <<EOF
[base]
id = 2
name = "arceos-aicp-dual-qemu"
vm_type = 1
cpu_num = 1
phys_cpu_ids = [${rtos_vcpu0_pcpu}]
phys_cpu_sets = [${rtos_vcpu0_mask}]

[kernel]
entry_point = 0xC020_0000
image_location = "memory"
kernel_path = "${arceos_bin}"
kernel_load_addr = 0xC020_0000
dtb_path = "${rtos_dtb}"
dtb_load_addr = 0xC000_0000
memory_regions = [
  # QEMU passthrough VirtIO DMA uses guest physical addresses directly, so the
  # RTOS RAM is an identity-mapped host reserved-memory region.
  [0xC000_0000, 0x1000_0000, 0x7, 2],
]

[devices]
interrupt_mode = "passthrough"
passthrough_devices = [
  ["/virtio_mmio@a003a00"],
]
passthrough_addresses = [
  [0x0900_0000, 0x1000],
]
excluded_devices = []
emu_devices = [
  ["gppt-gicd", 0x0800_0000, 0x1_0000, 0, 0x21, []],
  ["gppt-gicr", 0x080a_0000, 0x2_0000, 0, 0x20, [1, 0x2_0000, ${rtos_vcpu0_pcpu}]],
]
EOF
}

for tool in qemu-system-aarch64 dtc fdtoverlay fdtget cpio gzip; do
  require_tool "${tool}"
done
if [[ ! -f "${linux_kernel}" ]]; then
  echo "ERROR: Linux kernel not found at ${linux_kernel}" >&2
  echo "Run: cargo xtask image pull qemu-aarch64 -o tmp/images" >&2
  exit 1
fi

echo "[ai-rtos] Building ${client_impl} Linux guest initramfs"
build_linux_initramfs

echo "[ai-rtos] Building ArceOS RTOS guest"
(
  cd "${repo_root}"
  AX_IP=10.0.3.2 AX_GW=0.0.0.0 AX_PREFIX_LEN=24 \
    cargo xtask arceos build \
      -p arceos-aicp-server \
      --arch aarch64 \
      --config apps/arceos/build-aarch64-unknown-none-softfloat.toml
)
if [[ ! -s "${arceos_bin}" ]]; then
  echo "ERROR: ArceOS AICP image is missing or empty: ${arceos_bin}" >&2
  exit 1
fi

echo "[ai-rtos] Generating isolated Linux and ArceOS DTBs"
crop_virtio_nodes "${linux_src_dts}" "${linux_dts}.devices" "virtio_mmio@a003c00"
remove_dts_nodes "${linux_dts}.devices" "${linux_dts}.pruned1" "gpio-keys|pl061@"
remove_dts_nodes "${linux_dts}.pruned1" "${linux_dts}.pruned2" "fw-cfg@|pl031@|flash@"
remove_dts_nodes "${linux_dts}.pruned2" "${linux_dts}" "platform-bus@|pmu|its@"
rm -f "${linux_dts}.devices" "${linux_dts}.pruned1" "${linux_dts}.pruned2"
patch_linux_guest_console "${linux_dts}"
patch_bootargs "${linux_dts}" "console=ttyAMA0 earlycon=pl011,0x9040000 rdinit=/init panic=-1 loglevel=7 aicp.iterations=${iterations} aicp.mode=${mode} aicp.connect_retries=120"

crop_virtio_nodes "${linux_src_dts}" "${rtos_dts}.devices" "virtio_mmio@a003a00"
remove_dts_nodes "${rtos_dts}.devices" "${rtos_dts}.pruned1" "gpio-keys|pl061@"
remove_dts_nodes "${rtos_dts}.pruned1" "${rtos_dts}.pruned2" "fw-cfg@|pl031@|flash@"
remove_dts_nodes "${rtos_dts}.pruned2" "${rtos_dts}" "platform-bus@|pmu|its@"
rm -f "${rtos_dts}.devices" "${rtos_dts}.pruned1" "${rtos_dts}.pruned2"
perl -0pi -e \
  's/memory\@80000000/memory\@c0000000/; s/<0x00 0x80000000 0x00 0x40000000>/<0x00 0xc0000000 0x00 0x10000000>/' \
  "${rtos_dts}"
patch_bootargs "${rtos_dts}" ""

dtc -I dts -O dtb -o "${linux_dtb}" "${linux_dts}"
dtc -I dts -O dtb -o "${rtos_dtb}" "${rtos_dts}"
build_host_dtb
write_linux_vm_config
write_rtos_vm_config

cp "${qemu_template}" "${qemu_config}"
rm -f "${linux_console_log}"
LINUX_CONSOLE_LOG="${linux_console_log}" perl -0pi -e \
  's#  "-nographic",#  "-display",\n  "none",\n  "-monitor",\n  "none",\n  "-serial",\n  "stdio",\n  "-serial",\n  "file:$ENV{LINUX_CONSOLE_LOG}",#' \
  "${qemu_config}"
HOST_DTB="${host_dtb}" perl -0pi -e \
  's#  "-append",#  "-dtb",\n  "$ENV{HOST_DTB}",\n  "-append",#' \
  "${qemu_config}"

echo "[ai-rtos] Booting AxVisor Linux + ArceOS dual guest; log: ${log_file}"
(
  cd "${repo_root}"
  aicp_exec_new_session cargo xtask axvisor qemu \
    --config "${axvisor_board_config}" \
    --qemu-config "${qemu_config}" \
    --vmconfigs "${rtos_vm}" \
    --vmconfigs "${linux_vm}"
) > "${log_file}" 2>&1 &
qemu_pid=$!

aicp_wait_for_arceos_ready_in_logs \
  "$((SECONDS + boot_timeout_s))" "${qemu_pid}" 180 \
  2 "${log_file}" "${linux_console_log}"
if [[ "${client_impl}" == "rust" ]]; then
  wait_for_marker "AICP_RUST_GUEST_INIT begin=1"
  wait_for_marker "AICP_RUST_GUEST_NETCFG step=SIOCSIFFLAGS ret=0"
  wait_for_any_marker "Rust AICP connection" \
    "AICP_RUST_CONNECTED" \
    "AICP client connected:" \
    "AICP HELLO"
  wait_for_marker "AICP_RUST_DONE ok="
  done_token="AICP_RUST_DONE"
else
  wait_for_marker "AICP Linux guest client starting"
  wait_for_any_marker "Linux AICP connection" \
    "AICP Linux guest connected" \
    "AICP client connected:" \
    "AICP HELLO"
  wait_for_marker "AICP_LINUX_DONE ok="
  done_token="AICP_LINUX_DONE"
fi

if ! LC_ALL=C grep -a -q "${done_token} ok=.*failed=0" "${log_file}" "${linux_console_log}"; then
  echo "[ai-rtos] FAIL: Linux guest AICP client reported failures" >&2
  tail -n 180 "${log_file}" >&2 || true
  tail -n 100 "${linux_console_log}" >&2 || true
  exit 1
fi
if ! LC_ALL=C grep -a -q "CONTROL seq=" "${log_file}"; then
  echo "[ai-rtos] FAIL: ArceOS guest did not execute control messages" >&2
  tail -n 180 "${log_file}" >&2 || true
  exit 1
fi
if LC_ALL=C grep -a -Eq 'VM\[[0-9]+\].*(Fault|BadState)|emu_device mmio .* failed' "${log_file}"; then
  echo "[ai-rtos] FAIL: AxVisor reported a guest fault" >&2
  tail -n 180 "${log_file}" >&2 || true
  exit 1
fi

LC_ALL=C grep -a "${done_token}" "${log_file}" "${linux_console_log}" | tail -n 1
echo "[ai-rtos] PASS: Linux (${client_impl}) and ArceOS completed the AICP TCP/IP closed loop"
echo "[ai-rtos] AxVisor log: ${log_file}"
echo "[ai-rtos] Linux console log: ${linux_console_log}"
echo "log=${log_file}"
echo "linux_console_log=${linux_console_log}"
