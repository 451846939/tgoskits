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
Both guests communicate through AxVisor's isolated internal layer-2 switch.
Set AICP_QEMU_GDB_PORT to expose QEMU's GDB stub without stopping the boot.
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
source "${repo_root}/scripts/ai-rtos/lib/arceos.sh"
source "${repo_root}/scripts/ai-rtos/lib/markers.sh"
source "${repo_root}/scripts/ai-rtos/lib/process.sh"

demo_dir="${repo_root}/apps/ai-rtos-demo"
out_dir="${repo_root}/tmp/ai-rtos"
log_dir="${out_dir}/logs"
bundle_dir="${repo_root}/tmp/images/qemu-aarch64"
mkdir -p "${out_dir}" "${log_dir}" "${demo_dir}/build/aarch64"

stress_procs="${AICP_STRESS_PROCS:-0}"
qemu_gdb_port="${AICP_QEMU_GDB_PORT:-}"
axvisor_board_config="${AICP_AXVISOR_BOARD_CONFIG:-os/axvisor/configs/board/qemu-aarch64-ai-rtos.toml}"
if ! [[ "${stress_procs}" =~ ^[0-9]+$ ]] || (( stress_procs > 16 )); then
  echo "ERROR: AICP_STRESS_PROCS must be an integer in [0, 16], got '${stress_procs}'" >&2
  exit 2
fi
if [[ -n "${qemu_gdb_port}" ]] &&
   { ! [[ "${qemu_gdb_port}" =~ ^[0-9]+$ ]] ||
     (( qemu_gdb_port < 1024 || qemu_gdb_port > 65535 )); }; then
  echo "ERROR: AICP_QEMU_GDB_PORT must be an integer in [1024, 65535]" >&2
  exit 2
fi
aicp_configure_dual_guest_cpu_topology

stamp="$(date +%Y%m%d-%H%M%S)"
run_name="axvisor-dual-guest-aicp-${client_impl}"
log_file="${log_dir}/${run_name}-${stamp}.log"
linux_vm="${out_dir}/${run_name}-linux.generated.toml"
rtos_vm="${out_dir}/${run_name}-arceos.generated.toml"
qemu_config="${out_dir}/${run_name}-qemu.generated.toml"
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
    "${qemu_pid}" 180 "${log_file}"
}

wait_for_any_marker() {
  local description="$1"
  shift
  aicp_wait_for_any_marker_in_logs \
    "${description}" "$((SECONDS + boot_timeout_s))" "${qemu_pid}" 180 \
    1 "${log_file}" "$@"
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

write_linux_vm_config() {
  cat > "${linux_vm}" <<EOF
[base]
id = 2
name = "linux-ai-dual-qemu"
guest_type = "virtualized"
cpu_num = 2
phys_cpu_ids = [${linux_vcpu0_pcpu}, ${linux_vcpu1_pcpu}]
phys_cpu_sets = [${linux_vcpu0_mask}, ${linux_vcpu1_mask}]

[kernel]
entry_point = 0x8020_0000
image_location = "memory"
kernel_path = "${linux_kernel}"
kernel_load_addr = 0x8020_0000
dtb_load_addr = 0x8000_0000
ramdisk_path = "${initramfs}"
ramdisk_load_addr = 0x9000_0000
cmdline = "console=ttyAMA0 earlycon=pl011,0x09000000 rdinit=/init panic=-1 loglevel=7 aicp.iterations=${iterations} aicp.mode=${mode} aicp.connect_retries=120"
memory_regions = [
  [0x8000_0000, 0x2000_0000, 0x7, 0],
]

[devices]
passthrough = []
disabled = []

[[devices.virtual]]
id = "virtnet0"
model = "virtio-net"
guest_mac = [0x52, 0x54, 0x00, 0xaa, 0x03, 0x03]
EOF
}

write_rtos_vm_config() {
  cat > "${rtos_vm}" <<EOF
[base]
id = 1
name = "arceos-aicp-dual-qemu"
guest_type = "virtualized"
cpu_num = 1
phys_cpu_ids = [${rtos_vcpu0_pcpu}]
phys_cpu_sets = [${rtos_vcpu0_mask}]

[kernel]
entry_point = 0x8020_0000
image_location = "memory"
kernel_path = "${arceos_bin}"
kernel_load_addr = 0x8020_0000
dtb_load_addr = 0x8000_0000
memory_regions = [
  [0x8000_0000, 0x2000_0000, 0x7, 0],
]

[devices]
passthrough = []
disabled = []

[[devices.virtual]]
id = "virtnet0"
model = "virtio-net"
guest_mac = [0x52, 0x54, 0x00, 0xaa, 0x03, 0x02]
EOF
}

write_qemu_config() {
  local gdb_args=""
  if [[ -n "${qemu_gdb_port}" ]]; then
    gdb_args="  \"-gdb\", \"tcp::${qemu_gdb_port}\","
  fi
  cat > "${qemu_config}" <<EOF
args = [
  "-nographic",
  "-cpu", "cortex-a72",
  "-machine", "virt,virtualization=on,gic-version=3",
  "-smp", "${host_cpus}",
  "-m", "4g",
${gdb_args}
]
fail_regex = []
success_regex = []
timeout = ${boot_timeout_s}
to_bin = true
uefi = false
EOF
}

for tool in qemu-system-aarch64 cpio gzip; do
  require_tool "${tool}"
done
if [[ ! -f "${linux_kernel}" ]]; then
  echo "ERROR: Linux kernel not found at ${linux_kernel}" >&2
  echo "Run: cargo xtask image pull qemu-aarch64 --extract-dir tmp/images" >&2
  exit 1
fi

echo "[ai-rtos] Building ${client_impl} Linux guest initramfs"
build_linux_initramfs

echo "[ai-rtos] Building ArceOS RTOS guest"
AX_IP=10.0.3.2 AX_GW=0.0.0.0 AX_PREFIX_LEN=24 \
  aicp_build_arceos_guest \
    "${repo_root}" apps/arceos/build-aarch64-unknown-none-softfloat.toml
if [[ ! -s "${arceos_bin}" ]]; then
  echo "ERROR: ArceOS AICP image is missing or empty: ${arceos_bin}" >&2
  exit 1
fi

echo "[ai-rtos] Generating virtualized Linux and ArceOS VM configs"
write_linux_vm_config
write_rtos_vm_config
write_qemu_config

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
  1 "${log_file}"
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

if ! LC_ALL=C grep -a -q "${done_token} ok=.*failed=0" "${log_file}"; then
  echo "[ai-rtos] FAIL: Linux guest AICP client reported failures" >&2
  tail -n 180 "${log_file}" >&2 || true
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

LC_ALL=C grep -a "${done_token}" "${log_file}" | tail -n 1
echo "[ai-rtos] PASS: Linux (${client_impl}) and ArceOS completed the AICP TCP/IP closed loop"
echo "[ai-rtos] AxVisor log: ${log_file}"
echo "log=${log_file}"
