#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

configure_dual_guest_cpu_topology() {
  host_cpus="${AICP_HOST_CPUS:-4}"
  housekeeping_pcpu="${AICP_HOUSEKEEPING_PCPU:-0}"
  rtos_vcpu0_pcpu="${AICP_RTOS_VCPU0_PCPU:-1}"
  linux_vcpu0_pcpu="${AICP_LINUX_VCPU0_PCPU:-2}"
  linux_vcpu1_pcpu="${AICP_LINUX_VCPU1_PCPU:-3}"

  local name value pcpu
  for name in host_cpus housekeeping_pcpu rtos_vcpu0_pcpu linux_vcpu0_pcpu linux_vcpu1_pcpu; do
    value="${!name}"
    [[ "${value}" =~ ^[0-9]+$ ]] || {
      echo "ERROR: CPU topology values must be non-negative integers; ${name}='${value}'" >&2
      return 2
    }
  done
  ((host_cpus >= 4 && host_cpus <= 63)) || {
    echo "ERROR: AICP_HOST_CPUS must be in [4, 63], got '${host_cpus}'" >&2
    return 2
  }
  for pcpu in "${housekeeping_pcpu}" "${rtos_vcpu0_pcpu}" \
    "${linux_vcpu0_pcpu}" "${linux_vcpu1_pcpu}"; do
    ((pcpu < host_cpus)) || {
      echo "ERROR: pCPU ${pcpu} is outside AICP_HOST_CPUS=${host_cpus}" >&2
      return 2
    }
  done
  ((housekeeping_pcpu != rtos_vcpu0_pcpu && housekeeping_pcpu != linux_vcpu0_pcpu &&
    housekeeping_pcpu != linux_vcpu1_pcpu && rtos_vcpu0_pcpu != linux_vcpu0_pcpu &&
    rtos_vcpu0_pcpu != linux_vcpu1_pcpu && linux_vcpu0_pcpu != linux_vcpu1_pcpu)) || {
    echo "ERROR: housekeeping, RTOS, and AI guest vCPUs must use distinct host pCPUs" >&2
    return 2
  }
  ((linux_vcpu1_pcpu == linux_vcpu0_pcpu + 1)) || {
    echo "ERROR: the two AI guest pCPUs must be consecutive for the GPPT GICR range" >&2
    return 2
  }
  linux_vcpu0_mask=$((1 << linux_vcpu0_pcpu))
  linux_vcpu1_mask=$((1 << linux_vcpu1_pcpu))
  rtos_vcpu0_mask=$((1 << rtos_vcpu0_pcpu))
}

print_log_tails() {
  local tail_lines="$1"
  shift
  local log_file
  for log_file in "$@"; do
    echo "[ai-rtos] log tail: ${log_file}" >&2
    tail -n "${tail_lines}" "${log_file}" >&2 || true
  done
}

logs_match() {
  local pattern="$1"
  local extended="$2"
  shift 2
  local log_file
  for log_file in "$@"; do
    if [[ "${extended}" == true ]] && LC_ALL=C grep -a -E -q "${pattern}" "${log_file}" 2>/dev/null; then
      return 0
    fi
    if [[ "${extended}" == false ]] && LC_ALL=C grep -a -q "${pattern}" "${log_file}" 2>/dev/null; then
      return 0
    fi
  done
  return 1
}

wait_for_markers() {
  local description="$1"
  local deadline="$2"
  local qemu_process="$3"
  local tail_lines="$4"
  local log_count="$5"
  shift 5
  local log_files=("${@:1:log_count}")
  shift "${log_count}"
  local markers=("$@") marker

  while ((SECONDS < deadline)); do
    for marker in "${markers[@]}"; do
      if logs_match "${marker}" false "${log_files[@]}"; then
        echo "[ai-rtos] observed: ${marker}"
        return 0
      fi
    done
    if ! kill -0 "${qemu_process}" 2>/dev/null; then
      echo "[ai-rtos] AxVisor exited before marker group: ${description}" >&2
      print_log_tails "${tail_lines}" "${log_files[@]}"
      return 1
    fi
    sleep 1
  done
  echo "[ai-rtos] timeout waiting for marker group: ${description}" >&2
  print_log_tails "${tail_lines}" "${log_files[@]}"
  return 1
}

exec_new_session() {
  if command -v setsid >/dev/null 2>&1; then
    exec setsid "$@"
  fi
  if command -v perl >/dev/null 2>&1; then
    exec perl -MPOSIX -e 'POSIX::setsid() >= 0 or die "setsid: $!\n"; exec @ARGV or die "exec: $!\n"' -- "$@"
  fi
  exec "$@"
}

cleanup_process_group() {
  local process_id="${1:-}"
  [[ -n "${process_id}" ]] || return 0
  kill -TERM -- "-${process_id}" 2>/dev/null || true
  local deadline=$((SECONDS + 5))
  while kill -0 "${process_id}" 2>/dev/null && ((SECONDS < deadline)); do
    sleep 1
  done
  kill -KILL -- "-${process_id}" 2>/dev/null || true
  wait "${process_id}" 2>/dev/null || true
}

usage() {
  cat <<'EOF'
Usage: scripts/ai-rtos/runners/run_axvisor_dual_guest_aicp.sh [iterations] [ai|fixed] [boot_timeout_seconds]

Boot an AArch64 AxVisor QEMU instance with a two-vCPU Linux guest and an
ArceOS control guest. The guests exchange AICP frames over AxVisor's virtual
Ethernet switch: Linux is 10.0.3.3/24 and ArceOS is 10.0.3.2/24.
EOF
}

if [[ $# -gt 3 || "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage >&2
  exit 2
fi

iterations="${1:-20}"
mode="${2:-ai}"
boot_timeout_s="${3:-180}"
if ! [[ "${iterations}" =~ ^[1-9][0-9]*$ ]] || \
   ! [[ "${boot_timeout_s}" =~ ^[1-9][0-9]*$ ]] || \
   { [[ "${mode}" != "ai" ]] && [[ "${mode}" != "fixed" ]]; }; then
  usage >&2
  exit 2
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"

demo_dir="${repo_root}/apps/ai-rtos-demo"
out_dir="${repo_root}/tmp/ai-rtos"
log_dir="${out_dir}/logs"
bundle_dir="${repo_root}/tmp/images/qemu-aarch64"
linux_kernel="${bundle_dir}/linux/linux-qemu"
arceos_elf="${repo_root}/target/aarch64-unknown-linux-musl/release/arceos-aicp-server"
arceos_bin="${arceos_elf}.bin"
mkdir -p "${out_dir}" "${log_dir}" "${demo_dir}/build/aarch64"

configure_dual_guest_cpu_topology
stamp="$(date +%Y%m%d-%H%M%S)"
run_name="axvisor-linux-arceos-aicp"
log_file="${log_dir}/${run_name}-${stamp}.log"
linux_console_log="${log_dir}/${run_name}-linux-${stamp}.log"
linux_vm="${out_dir}/${run_name}-linux-${stamp}.toml"
arceos_vm="${out_dir}/${run_name}-arceos-${stamp}.toml"
qemu_config="${out_dir}/${run_name}-qemu-${stamp}.toml"
initramfs_dir="${out_dir}/${run_name}-initramfs-${stamp}"
initramfs="${out_dir}/${run_name}-initramfs-${stamp}.cpio.gz"
qemu_pid=""

cleanup() {
  cleanup_process_group "${qemu_pid}" || true
}
trap cleanup EXIT

require_tool() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "ERROR: missing required tool: $1" >&2
    exit 1
  }
}

prepare_arceos_binary() {
  local objcopy=""
  local candidate
  for candidate in aarch64-linux-musl-objcopy aarch64-linux-gnu-objcopy rust-objcopy llvm-objcopy objcopy; do
    if command -v "${candidate}" >/dev/null 2>&1; then
      objcopy="${candidate}"
      break
    fi
  done
  [[ -n "${objcopy}" ]] || { echo "ERROR: no ELF-to-binary objcopy tool is available" >&2; exit 1; }
  [[ -s "${arceos_elf}" ]] || { echo "ERROR: missing ArceOS ELF: ${arceos_elf}" >&2; exit 1; }
  "${objcopy}" -O binary "${arceos_elf}" "${arceos_bin}"
  [[ -s "${arceos_bin}" ]] || { echo "ERROR: failed to produce ArceOS binary" >&2; exit 1; }
}

prepare_linux_initramfs() {
  rm -rf "${initramfs_dir}"
  mkdir -p "${initramfs_dir}"
  make -C "${demo_dir}" linux-init-aarch64 \
    CFLAGS="-O2 -g -Wall -Wextra -Werror -std=c11 -DAICP_INIT_ITERATIONS=${iterations}u -DAICP_INIT_MODE=\\\"${mode}\\\" -DAICP_INIT_STRESS_PROCS=0u -DAICP_INIT_TCP_TIMEOUT_MS=15000u"
  cp "${demo_dir}/build/aarch64/aicp_init" "${initramfs_dir}/init"
  chmod +x "${initramfs_dir}/init"
  (cd "${initramfs_dir}" && find . -print | cpio -o -H newc | gzip -9 > "${initramfs}")
}

write_guest_configs() {
  cat > "${linux_vm}" <<EOF
[base]
id = 1
name = "linux-aicp"
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
cmdline = "console=ttyAMA0 earlycon rdinit=/init panic=-1 loglevel=7 aicp.iterations=${iterations} aicp.mode=${mode}"
memory_regions = [[0x8000_0000, 0x2000_0000, 0x7, 0]]

[devices]
passthrough = []
disabled = []
[[devices.virtual]]
id = "aicp-net"
model = "virtio-net"
guest_mac = [0x52, 0x54, 0x00, 0xaa, 0x03, 0x03]
EOF

  cat > "${arceos_vm}" <<EOF
[base]
id = 2
name = "arceos-aicp"
guest_type = "virtualized"
cpu_num = 1
phys_cpu_ids = [${rtos_vcpu0_pcpu}]
phys_cpu_sets = [${rtos_vcpu0_mask}]

[kernel]
entry_point = 0xC020_0000
image_location = "memory"
kernel_path = "${arceos_bin}"
kernel_load_addr = 0xC020_0000
dtb_load_addr = 0xC000_0000
memory_regions = [[0xC000_0000, 0x1000_0000, 0x7, 0]]

[devices]
passthrough = []
disabled = []
[[devices.virtual]]
id = "aicp-net"
model = "virtio-net"
guest_mac = [0x52, 0x54, 0x00, 0xaa, 0x03, 0x02]
EOF
}

wait_marker() {
  wait_for_markers "$1" "$((SECONDS + boot_timeout_s))" \
    "${qemu_pid}" 180 2 "${log_file}" "${linux_console_log}" "$1"
}

for tool in cpio gzip perl; do require_tool "${tool}"; done
if [[ ! -s "${linux_kernel}" ]]; then
  echo "ERROR: Linux image missing: ${linux_kernel}" >&2
  echo "Run: cargo xtask image pull qemu-aarch64 --extract-dir tmp/images" >&2
  exit 1
fi

echo "[ai-rtos] Building Linux guest initramfs"
prepare_linux_initramfs
echo "[ai-rtos] Building ArceOS control guest"
(cd "${repo_root}" && cargo xtask arceos build -p arceos-aicp-server --arch aarch64 --config apps/arceos/aicp-server/build-aarch64-unknown-none-softfloat-dual-guest.toml)
prepare_arceos_binary
write_guest_configs

cp "${repo_root}/os/axvisor/configs/qemu/qemu-aarch64-aicp-dual-net.toml" "${qemu_config}"
rm -f "${linux_console_log}"
AXVISOR_CONSOLE_LOG="${log_file}" LINUX_CONSOLE_LOG="${linux_console_log}" perl -0pi -e \
  's#  "-nographic",#  "-display",\n  "none",\n  "-monitor",\n  "none",\n  "-serial",\n  "file:$ENV{AXVISOR_CONSOLE_LOG}",\n  "-serial",\n  "file:$ENV{LINUX_CONSOLE_LOG}",#' \
  "${qemu_config}"

echo "[ai-rtos] Booting AxVisor: Linux(2 vCPU) + ArceOS"
(cd "${repo_root}" && exec_new_session cargo xtask axvisor qemu \
  --config os/axvisor/configs/board/qemu-aarch64-aicp-dual.toml \
  --qemu-config "${qemu_config}" --vmconfigs "${arceos_vm}" --vmconfigs "${linux_vm}" > "${log_file}" 2>&1) &
qemu_pid=$!

wait_for_markers "ArceOS AICP server ready" "$((SECONDS + boot_timeout_s))" \
  "${qemu_pid}" 180 2 "${log_file}" "${linux_console_log}" \
  "AICP_RTOS_READY" \
  "AICP ArceOS RTOS TCP server listening" \
  "AICP ArceOS RTOS UDP server listening" \
  "AICP ArceOS RTOS server listening" \
  "AICP client connected:"
wait_marker "AICP_RTOS_NET_READY iface=eth0 ip=10.0.3.2/24"
wait_marker "AICP Linux guest client starting"
wait_marker "AICP_LINUX_DONE ok="
if ! logs_match 'AICP_LINUX_DONE ok=[0-9]+ failed=0' true "${log_file}" "${linux_console_log}"; then
  echo "[ai-rtos] FAIL: Linux client reported failures" >&2
  print_log_tails 180 "${log_file}" "${linux_console_log}"
  exit 1
fi
if ! logs_match "CONTROL seq=" false "${log_file}"; then
  echo "[ai-rtos] FAIL: ArceOS did not execute control messages" >&2
  print_log_tails 180 "${log_file}"
  exit 1
fi
if logs_match 'VM\[[0-9]+\].*(Fault|BadState)|emu_device mmio .* failed' true "${log_file}"; then
  echo "[ai-rtos] FAIL: AxVisor reported a guest fault" >&2
  print_log_tails 180 "${log_file}"
  exit 1
fi

grep -a 'AICP_LINUX_DONE' "${log_file}" "${linux_console_log}" | tail -n 1
echo "[ai-rtos] PASS: Linux-to-ArceOS AICP TCP/IP control loop completed"
echo "log=${log_file}"
echo "linux_console_log=${linux_console_log}"
