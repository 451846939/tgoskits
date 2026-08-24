#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
#
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  scripts/ai-rtos/runners/run_axvisor_dual_guest_aicp.sh [iterations] [ai|fixed] [boot_timeout_seconds]

Boots two AxVisor guests in one QEMU AArch64 run:
  Linux AI guest    10.0.3.3/24, 2 vCPUs pinned to pCPU2,pCPU3
  RTOS guest        10.0.3.2/24, 1 vCPU pinned to pCPU1

The default four-core layout reserves pCPU0 for AxVisor housekeeping. Override
the topology with AICP_HOST_CPUS, AICP_LINUX_VCPU0_PCPU,
AICP_LINUX_VCPU1_PCPU, and AICP_RTOS_VCPU0_PCPU.

Set AICP_CLIENT_IMPL=c or AICP_CLIENT_IMPL=rust to select the Linux client.
Set AICP_RTOS_GUEST=arceos, freertos, rtthread, or zephyr to select the control guest.
ArceOS and FreeRTOS use AxVisor's isolated virtual switch. Zephyr uses the
QEMU hub-backed direct VirtIO-MMIO compatibility path, as does RT-Thread.
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
rtos_guest="${AICP_RTOS_GUEST:-arceos}"

if [[ "${mode}" != "ai" && "${mode}" != "fixed" ]]; then
  usage >&2
  exit 2
fi
if [[ "${client_impl}" != "c" && "${client_impl}" != "rust" ]]; then
  echo "ERROR: AICP_CLIENT_IMPL must be c or rust, got '${client_impl}'" >&2
  exit 2
fi
if [[ "${rtos_guest}" != "arceos" && "${rtos_guest}" != "freertos" && \
      "${rtos_guest}" != "rtthread" && "${rtos_guest}" != "zephyr" ]]; then
  echo "ERROR: AICP_RTOS_GUEST must be arceos, freertos, rtthread, or zephyr, got '${rtos_guest}'" >&2
  exit 2
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
source "${repo_root}/scripts/ai-rtos/lib/cpu_topology.sh"
source "${repo_root}/scripts/ai-rtos/lib/markers.sh"
source "${repo_root}/scripts/ai-rtos/lib/process.sh"

demo_dir="${repo_root}/apps/ai-rtos-demo"
out_dir="${repo_root}/tmp/ai-rtos"
log_dir="${out_dir}/logs"
bundle_dir="${repo_root}/tmp/images/qemu-aarch64"
mkdir -p "${out_dir}" "${log_dir}" "${demo_dir}/build/aarch64"

stress_procs="${AICP_STRESS_PROCS:-0}"
axvisor_board_config="${AICP_AXVISOR_BOARD_CONFIG:-os/axvisor/configs/board/qemu-aarch64-aicp-dual.toml}"
if ! [[ "${stress_procs}" =~ ^[0-9]+$ ]] || (( stress_procs > 16 )); then
  echo "ERROR: AICP_STRESS_PROCS must be an integer in [0, 16], got '${stress_procs}'" >&2
  exit 2
fi
aicp_configure_dual_guest_cpu_topology

stamp="$(date +%Y%m%d-%H%M%S)"
run_name="axvisor-dual-guest-aicp-${client_impl}"
log_file="${log_dir}/${run_name}-${stamp}.log"
linux_console_log="${log_dir}/${run_name}-linux-console-${stamp}.log"
linux_vm="${out_dir}/${run_name}-linux.generated.toml"
rtos_vm="${out_dir}/${run_name}-${rtos_guest}.generated.toml"
qemu_config="${out_dir}/${run_name}-qemu.generated.toml"
qemu_template="${repo_root}/os/axvisor/configs/qemu/qemu-aarch64-aicp-dual-net.toml"
direct_host_dtb="${out_dir}/${run_name}-direct-host.dtb"
direct_host_base_dtb="${out_dir}/${run_name}-direct-host-base.dtb"
direct_host_overlay_dts="${out_dir}/${run_name}-direct-reserved-memory-overlay.dts"
direct_host_overlay_dtbo="${out_dir}/${run_name}-direct-reserved-memory.dtbo"
initramfs_dir="${out_dir}/${run_name}-initramfs"
initramfs="${out_dir}/${run_name}-initramfs.cpio.gz"
linux_kernel="${bundle_dir}/linux/linux-qemu"
arceos_bin="${repo_root}/target/aarch64-unknown-linux-musl/release/arceos-aicp-server.bin"
arceos_build_config="apps/arceos/aicp-server/build-aarch64-unknown-none-softfloat.toml"
freertos_bin="${out_dir}/build-freertos-aicp/aicp-freertos.bin"
rtthread_build_dir="${AICP_RTTHREAD_BUILD_DIR:-${out_dir}/build-rtthread-aicp-direct}"
rtthread_bin="${AICP_RTTHREAD_BIN:-${rtthread_build_dir}/rtthread.bin}"
zephyr_build_dir="${AICP_ZEPHYR_BUILD_DIR:-${out_dir}/build-zephyr-aicp-direct}"
zephyr_bin="${AICP_ZEPHYR_BIN:-${zephyr_build_dir}/zephyr/zephyr.bin}"

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

prepare_direct_virtio_host_dtb() {
  local memory_base="$1"
  local memory_size="$2"
  local node_name="$3"

  for tool in qemu-system-aarch64 dtc fdtoverlay; do
    require_tool "${tool}"
  done

  rm -f "${direct_host_base_dtb}" "${direct_host_overlay_dtbo}" "${direct_host_dtb}"
  qemu-system-aarch64 \
    -display none \
    -monitor none \
    -serial null \
    -cpu cortex-a72 \
    -machine "virt,virtualization=on,gic-version=3,dumpdtb=${direct_host_base_dtb}" \
    -smp "${host_cpus}" \
    -m 8g

  cat > "${direct_host_overlay_dts}" <<EOF
/dts-v1/;
/plugin/;

/ {
    fragment@0 {
        target-path = "/";
        __overlay__ {
            reserved-memory {
                #address-cells = <2>;
                #size-cells = <2>;
                ranges;
                ${node_name}@${memory_base#0x} {
                    reg = <0x0 ${memory_base} 0x0 ${memory_size}>;
                    no-map;
                };
            };
        };
    };
};
EOF
  dtc -@ -I dts -O dtb -o "${direct_host_overlay_dtbo}" "${direct_host_overlay_dts}"
  fdtoverlay -i "${direct_host_base_dtb}" -o "${direct_host_dtb}" \
    "${direct_host_overlay_dtbo}"
}

prepare_arceos_raw_image() {
  local arceos_elf="${repo_root}/target/aarch64-unknown-linux-musl/release/arceos-aicp-server"
  local objcopy=""
  local candidate

  if [[ ! -s "${arceos_elf}" ]]; then
    echo "ERROR: ArceOS AICP ELF is missing or empty: ${arceos_elf}" >&2
    exit 1
  fi
  for candidate in aarch64-linux-musl-objcopy aarch64-linux-gnu-objcopy rust-objcopy llvm-objcopy objcopy; do
    if command -v "${candidate}" >/dev/null 2>&1; then
      objcopy="${candidate}"
      break
    fi
  done
  if [[ -z "${objcopy}" ]]; then
    echo "ERROR: no AArch64 ELF-to-binary objcopy tool is available" >&2
    exit 1
  fi

  "${objcopy}" -O binary "${arceos_elf}" "${arceos_bin}"
  if [[ ! -s "${arceos_bin}" ]]; then
    echo "ERROR: ArceOS AICP raw image is missing or empty after objcopy: ${arceos_bin}" >&2
    exit 1
  fi
  echo "[ai-rtos] Prepared ArceOS raw image with ${objcopy}: ${arceos_bin}"
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
  local memory_map_type="0"
  local device_config

  device_config='passthrough = []
disabled = []

[[devices.virtual]]
id = "aicp-net"
model = "virtio-net"
guest_mac = [0x52, 0x54, 0x00, 0xaa, 0x03, 0x03]'
  if [[ "${rtos_guest}" == "rtthread" || "${rtos_guest}" == "zephyr" ]]; then
    memory_map_type="2"
    device_config='passthrough = [
  { path = "/virtio_mmio@a003c00" },
]
disabled = []'
  fi

  cat > "${linux_vm}" <<EOF
[base]
id = 1
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
cmdline = "console=ttyAMA0 earlycon rdinit=/init panic=-1 loglevel=7 aicp.iterations=${iterations} aicp.mode=${mode} aicp.connect_retries=120"
memory_regions = [
  [0x8000_0000, 0x2000_0000, 0x7, ${memory_map_type}],
]

[devices]
${device_config}
EOF
}

write_rtos_vm_config() {
  local entry_point
  local kernel_path
  local kernel_load_addr
  local dtb_load_addr
  local memory_base
  local memory_size
  local device_config
  local guest_name

  case "${rtos_guest}" in
    arceos)
      entry_point="0xC020_0000"
      kernel_path="${arceos_bin}"
      kernel_load_addr="0xC020_0000"
      dtb_load_addr="0xC000_0000"
      memory_base="0xC000_0000"
      memory_size="0x1000_0000"
      memory_map_type="0"
      guest_name="arceos-aicp-dual-qemu"
      ;;
    freertos)
      entry_point="0xD000_1000"
      kernel_path="${freertos_bin}"
      kernel_load_addr="0xD000_0000"
      dtb_load_addr="0xD7E0_0000"
      memory_base="0xD000_0000"
      memory_size="0x0800_0000"
      memory_map_type="0"
      guest_name="freertos-aicp-dual-qemu"
      ;;
    rtthread)
      entry_point="0xC000_0000"
      kernel_path="${rtthread_bin}"
      kernel_load_addr="0xC000_0000"
      dtb_load_addr="0xCFE0_0000"
      memory_base="0xC000_0000"
      memory_size="0x1000_0000"
      memory_map_type="2"
      guest_name="rtthread-aicp-dual-qemu"
      ;;
    zephyr)
      entry_point="0xD000_1104"
      kernel_path="${zephyr_bin}"
      kernel_load_addr="0xD000_0000"
      dtb_load_addr="0xD7E0_0000"
      memory_base="0xD000_0000"
      memory_size="0x0800_0000"
      memory_map_type="2"
      guest_name="zephyr-aicp-dual-qemu"
      ;;
  esac

  device_config='passthrough = []
disabled = []

[[devices.virtual]]
id = "aicp-net"
model = "virtio-net"
guest_mac = [0x52, 0x54, 0x00, 0xaa, 0x03, 0x02]'
  if [[ "${rtos_guest}" == "rtthread" || "${rtos_guest}" == "zephyr" ]]; then
    device_config='passthrough = [
  { path = "/virtio_mmio@a003a00" },
]
disabled = []'
  fi

  cat > "${rtos_vm}" <<EOF
[base]
id = 2
name = "${guest_name}"
guest_type = "virtualized"
cpu_num = 1
phys_cpu_ids = [${rtos_vcpu0_pcpu}]
phys_cpu_sets = [${rtos_vcpu0_mask}]

[kernel]
entry_point = ${entry_point}
image_location = "memory"
kernel_path = "${kernel_path}"
kernel_load_addr = ${kernel_load_addr}
dtb_load_addr = ${dtb_load_addr}
memory_regions = [
  [${memory_base}, ${memory_size}, 0x7, ${memory_map_type}],
]

[devices]
${device_config}
EOF
}

for tool in cpio gzip; do
  require_tool "${tool}"
done
if [[ ! -f "${linux_kernel}" ]]; then
  echo "ERROR: Linux kernel not found at ${linux_kernel}" >&2
  echo "Run: cargo xtask image pull qemu-aarch64 --extract-dir tmp/images" >&2
  exit 1
fi

echo "[ai-rtos] Building ${client_impl} Linux guest initramfs"
build_linux_initramfs

case "${rtos_guest}" in
  arceos)
    echo "[ai-rtos] Building ArceOS RTOS guest"
    (
      cd "${repo_root}"
      cargo xtask arceos build \
        -p arceos-aicp-server \
        --arch aarch64 \
        --config "${arceos_build_config}"
    )
    prepare_arceos_raw_image
    ;;
  freertos)
    echo "[ai-rtos] Building FreeRTOS RTOS guest"
    "${repo_root}/scripts/ai-rtos/build/build_freertos_aicp_guest.sh"
    if [[ ! -s "${freertos_bin}" ]]; then
      echo "ERROR: FreeRTOS AICP binary is missing or empty: ${freertos_bin}" >&2
      exit 1
    fi
    ;;
  zephyr)
    if [[ "${AICP_ZEPHYR_SKIP_BUILD:-0}" != "1" ]]; then
      echo "[ai-rtos] Building Zephyr RTOS guest with direct VirtIO-MMIO"
      AICP_ZEPHYR_PROFILE=axvisor-direct-virtio \
        ZEPHYR_BUILD_DIR="${zephyr_build_dir}" \
        "${repo_root}/scripts/ai-rtos/build/build_zephyr_aicp_guest.sh"
    fi
    if [[ ! -s "${zephyr_bin}" ]]; then
      echo "ERROR: Zephyr AICP binary is missing or empty: ${zephyr_bin}" >&2
      exit 1
    fi
    ;;
  rtthread)
    if [[ "${AICP_RTTHREAD_SKIP_BUILD:-0}" != "1" ]]; then
      echo "[ai-rtos] Building RT-Thread RTOS guest with direct VirtIO-MMIO"
      RTTHREAD_BUILD_DIR="${rtthread_build_dir}" \
        RTTHREAD_RAM_BASE=0xC0000000 \
        RTTHREAD_TEXT_OFFSET=0x0 \
        RTTHREAD_GIC_VERSION=3 \
        RTTHREAD_VIRTIO_MMIO_BASE=0x0a003a00 \
        RTTHREAD_VIRTIO_MAX_NR=1 \
        RTTHREAD_VIRTIO_IRQ_BASE=77 \
        "${repo_root}/scripts/ai-rtos/build/build_rtthread_aicp_guest.sh"
    fi
    if [[ ! -s "${rtthread_bin}" ]]; then
      echo "ERROR: RT-Thread AICP binary is missing or empty: ${rtthread_bin}" >&2
      exit 1
    fi
    ;;
esac

echo "[ai-rtos] Generating AxVM guest configurations"
write_linux_vm_config
write_rtos_vm_config

if [[ "${rtos_guest}" == "rtthread" || "${rtos_guest}" == "zephyr" ]]; then
  qemu_template="${repo_root}/os/axvisor/configs/qemu/qemu-aarch64-aicp-dual-direct-virtio.toml"
  if [[ "${rtos_guest}" == "rtthread" ]]; then
    prepare_direct_virtio_host_dtb 0xc0000000 0x10000000 rtthread
  else
    prepare_direct_virtio_host_dtb 0xd0000000 0x08000000 zephyr
  fi
fi
cp "${qemu_template}" "${qemu_config}"
if [[ "${rtos_guest}" == "rtthread" || "${rtos_guest}" == "zephyr" ]]; then
  DIRECT_HOST_DTB="${direct_host_dtb}" perl -0pi -e \
    's#  "-machine",#  "-dtb",\n  "$ENV{DIRECT_HOST_DTB}",\n  "-machine",#' \
    "${qemu_config}"
fi
rm -f "${linux_console_log}"
AXVISOR_CONSOLE_LOG="${log_file}" LINUX_CONSOLE_LOG="${linux_console_log}" perl -0pi -e \
  's#  "-nographic",#  "-display",\n  "none",\n  "-monitor",\n  "none",\n  "-serial",\n  "file:$ENV{AXVISOR_CONSOLE_LOG}",\n  "-serial",\n  "file:$ENV{LINUX_CONSOLE_LOG}",#' \
  "${qemu_config}"

echo "[ai-rtos] Booting AxVisor Linux + ${rtos_guest} dual guest; log: ${log_file}"
pushd "${repo_root}" >/dev/null
aicp_exec_new_session cargo xtask axvisor qemu \
  --config "${axvisor_board_config}" \
  --qemu-config "${qemu_config}" \
  --vmconfigs "${rtos_vm}" \
  --vmconfigs "${linux_vm}" > "${log_file}" 2>&1 &
qemu_pid=$!
popd >/dev/null

case "${rtos_guest}" in
  arceos)
    aicp_wait_for_arceos_ready_in_logs \
      "$((SECONDS + boot_timeout_s))" "${qemu_pid}" 180 \
      2 "${log_file}" "${linux_console_log}"
    wait_for_marker "AICP_RTOS_NET_READY iface=eth0 ip=10.0.3.2/24"
    control_marker="CONTROL seq="
    ;;
  freertos)
    wait_for_marker "AICP_FREERTOS_FDT_VIRTIO base="
    wait_for_marker "AICP_FREERTOS_READY transport=tcp port=8800 ip=10.0.3.2"
    control_marker="AICP_FREERTOS_CONTROL seq="
    ;;
  rtthread)
    wait_for_marker "AICP_RTTHREAD_NET_UP"
    wait_for_marker "AICP_RTTHREAD_READY transport=tcp port=8800"
    control_marker="AICP_RTTHREAD_CONTROL seq="
    ;;
  zephyr)
    wait_for_marker "AICP_ZEPHYR_NET_UP"
    control_marker="AICP_ZEPHYR_CONTROL seq="
    ;;
esac
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
if ! LC_ALL=C grep -a -q "${control_marker}" "${log_file}"; then
  echo "[ai-rtos] FAIL: ${rtos_guest} guest did not execute control messages" >&2
  tail -n 180 "${log_file}" >&2 || true
  exit 1
fi
if LC_ALL=C grep -a -Eq 'VM\[[0-9]+\].*(Fault|BadState)|emu_device mmio .* failed' "${log_file}"; then
  echo "[ai-rtos] FAIL: AxVisor reported a guest fault" >&2
  tail -n 180 "${log_file}" >&2 || true
  exit 1
fi

LC_ALL=C grep -a "${done_token}" "${log_file}" "${linux_console_log}" | tail -n 1
echo "[ai-rtos] PASS: Linux (${client_impl}) and ${rtos_guest} completed the AICP TCP/IP closed loop"
echo "[ai-rtos] AxVisor log: ${log_file}"
echo "[ai-rtos] Linux console log: ${linux_console_log}"
echo "log=${log_file}"
echo "linux_console_log=${linux_console_log}"
