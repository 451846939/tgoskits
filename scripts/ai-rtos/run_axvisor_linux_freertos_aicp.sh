#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
#
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

usage() {
  cat <<'EOF'
用法：
  scripts/ai-rtos/run_axvisor_linux_freertos_aicp.sh [迭代次数] [ai|fixed] [启动超时秒数]

在同一个 QEMU/AxVisor 实例中启动：
  Linux Guest：2 vCPU，默认绑定 pCPU2、pCPU3，地址 10.0.3.3
  FreeRTOS Guest：1 vCPU，默认绑定 pCPU1，legacy virtio-net + FreeRTOS+TCP + AICP/TCP

两个 virtio-net 设备接入同一个隔离 QEMU 二层 hub。主数据通道是
TCP/IP，不使用 vsock、共享内存、HyperCall、hostfwd 或 NAT。
EOF
}

if [[ $# -gt 3 || "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage >&2
  exit 2
fi

iterations="${1:-20}"
mode="${2:-ai}"
boot_timeout_s="${3:-240}"
if ! [[ "${iterations}" =~ ^[0-9]+$ ]] || (( iterations < 1 )); then
  echo "ERROR: 迭代次数必须为正整数，实际为 '${iterations}'" >&2
  exit 2
fi
if [[ "${mode}" != "ai" && "${mode}" != "fixed" ]]; then
  usage >&2
  exit 2
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
source "${repo_root}/scripts/ai-rtos/lib/cpu_topology.sh"
source "${repo_root}/scripts/ai-rtos/lib/dtb.sh"
source "${repo_root}/scripts/ai-rtos/lib/host_tools.sh"
source "${repo_root}/scripts/ai-rtos/lib/process.sh"
demo_dir="${repo_root}/apps/ai-rtos-demo"
out_dir="${repo_root}/tmp/ai-rtos"
log_dir="${out_dir}/logs"
result_dir="${out_dir}/results/axvisor-linux-freertos"
bundle_dir="${repo_root}/tmp/images/qemu-aarch64"
stress_procs="${AICP_STRESS_PROCS:-0}"
skip_freertos_build="${AICP_SKIP_FREERTOS_BUILD:-0}"
qemu_trace="${AICP_QEMU_TRACE:-0}"
mkdir -p "${out_dir}" "${log_dir}" "${result_dir}" "${demo_dir}/build/aarch64"
aicp_configure_dual_guest_cpu_topology

if ! [[ "${stress_procs}" =~ ^[0-9]+$ ]] || (( stress_procs > 16 )); then
  echo "ERROR: AICP_STRESS_PROCS 必须在 [0, 16]，实际为 '${stress_procs}'" >&2
  exit 2
fi
if [[ "${skip_freertos_build}" != "0" && "${skip_freertos_build}" != "1" ]]; then
  echo "ERROR: AICP_SKIP_FREERTOS_BUILD 必须为 0 或 1，实际为 '${skip_freertos_build}'" >&2
  exit 2
fi
if [[ "${qemu_trace}" != "0" && "${qemu_trace}" != "1" ]]; then
  echo "ERROR: AICP_QEMU_TRACE 必须为 0 或 1，实际为 '${qemu_trace}'" >&2
  exit 2
fi

stamp="$(date +%Y%m%d-%H%M%S)"
log_file="${log_dir}/axvisor-linux-freertos-aicp-${stamp}.log"
linux_console_log="${log_dir}/linux-freertos-console-${stamp}.log"
trace_file="${log_dir}/axvisor-linux-freertos-virtio-${stamp}.trace"
trace_events="${out_dir}/qemu-linux-freertos-virtio-trace-events"
summary_file="${result_dir}/latest-summary.txt"
qemu_template="${repo_root}/os/axvisor/configs/qemu/qemu-aarch64-aicp-dual-net.toml"
qemu_config="${out_dir}/qemu-aarch64-linux-freertos.toml"
linux_src_dts="${repo_root}/os/axvisor/configs/vms/qemu/aarch64/linux-smp1.dts"
linux_dts="${out_dir}/linux-freertos-client.dts"
linux_dtb="${out_dir}/linux-freertos-client.dtb"
host_base_dtb="${out_dir}/qemu-aarch64-freertos-host-base.dtb"
host_overlay_dts="${repo_root}/configs/ai-rtos/qemu-aarch64-freertos-reserved-memory-overlay.dts"
host_overlay_dtbo="${out_dir}/qemu-aarch64-freertos-reserved-memory.dtbo"
host_dtb="${out_dir}/qemu-aarch64-freertos-host.dtb"
host_dtb_dummy_disk="${out_dir}/qemu-freertos-dtb-dummy.img"
linux_vm="${out_dir}/linux-freertos-client.generated.toml"
freertos_vm="${out_dir}/freertos-aicp.generated.toml"
initramfs_dir="${out_dir}/initramfs-linux-freertos"
initramfs="${out_dir}/linux-freertos-initramfs.cpio.gz"
linux_kernel="${bundle_dir}/linux/linux-qemu"
freertos_build_dir="${FREERTOS_BUILD_DIR:-${out_dir}/build-freertos-aicp}"
freertos_bin="${freertos_build_dir}/aicp-freertos.bin"
freertos_elf="${freertos_build_dir}/aicp-freertos.elf"

cleanup() {
  aicp_cleanup_process_tree "${qemu_pid:-}"
}
trap cleanup EXIT

wait_for_marker() {
  local marker="$1"
  while ((SECONDS < global_deadline)); do
    if LC_ALL=C grep -a -q "${marker}" "${log_file}" "${linux_console_log}" 2>/dev/null; then
      echo "[ai-rtos] 已观察到：${marker}"
      return 0
    fi
    if LC_ALL=C grep -a -Eq \
      'Unhandled synchronous exception from current EL|current vCPU is not set|AArch64 guest IRQ injection rejected' \
      "${log_file}" 2>/dev/null; then
      echo "[ai-rtos] FAIL：检测到 AxVisor Host/vCPU 异常，停止等待：${marker}" >&2
      tail -n 220 "${log_file}" >&2 || true
      tail -n 120 "${linux_console_log}" >&2 || true
      return 1
    fi
    if ! kill -0 "${qemu_pid}" 2>/dev/null; then
      echo "[ai-rtos] AxVisor 在出现标记前退出：${marker}" >&2
      tail -n 220 "${log_file}" >&2 || true
      tail -n 120 "${linux_console_log}" >&2 || true
      return 1
    fi
    sleep 1
  done
  echo "[ai-rtos] 等待标记超时：${marker}" >&2
  tail -n 220 "${log_file}" >&2 || true
  tail -n 120 "${linux_console_log}" >&2 || true
  return 1
}

build_host_dtb() {
  local tool
  for tool in qemu-system-aarch64 dtc fdtoverlay fdtget; do
    if ! command -v "${tool}" >/dev/null 2>&1; then
      echo "ERROR: 缺少设备树生成工具：${tool}" >&2
      exit 1
    fi
  done

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

  local freertos_reserved_reg
  freertos_reserved_reg="$(fdtget -tx "${host_dtb}" /reserved-memory/freertos@d0000000 reg)"
  if [[ "${freertos_reserved_reg}" != "0 d0000000 0 8000000" ]]; then
    echo "ERROR: 宿主 DTB 中 FreeRTOS 预留内存校验失败：${freertos_reserved_reg}" >&2
    exit 1
  fi
  echo "[ai-rtos] 宿主 DTB 已预留 FreeRTOS DMA 内存：${freertos_reserved_reg}"
}

write_linux_vm_config() {
  cat > "${linux_vm}" <<EOF
[base]
id = 1
name = "linux-ai-freertos-qemu"
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
  # Linux 使用 AxVisor 分配的身份映射内存。QEMU 直通 virtio 设备直接使用
  # virtqueue 中的 Guest PA 做 DMA，因此 Guest PA 必须同时是有效 Host PA。
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

write_freertos_vm_config() {
  cat > "${freertos_vm}" <<EOF
[base]
id = 2
name = "freertos-aicp-qemu"
vm_type = 1
cpu_num = 1
phys_cpu_ids = [${rtos_vcpu0_pcpu}]
phys_cpu_sets = [${rtos_vcpu0_mask}]

[kernel]
entry_point = 0xD000_1000
image_location = "memory"
kernel_path = "${freertos_bin}"
kernel_load_addr = 0xD000_0000
memory_regions = [
  [0xD000_0000, 0x0800_0000, 0x7, 2],
]

[devices]
interrupt_mode = "passthrough"
passthrough_devices = [
  ["/virtio_mmio@a003a00"],
]
# UART 仅用于轮询日志输出，不声明其中断所有权。宿主保留第一个 PL011
# 的 SPI，Linux Guest 使用独立的第二个 PL011，避免中断所有权冲突。
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

if [[ ! -f "${linux_kernel}" ]]; then
  echo "ERROR: 缺少 Linux kernel：${linux_kernel}" >&2
  echo "请先执行：cargo xtask image pull qemu-aarch64 --extract-dir tmp/images" >&2
  exit 1
fi

if [[ "${skip_freertos_build}" == "1" ]]; then
  echo "[ai-rtos] 复用已验证的 FreeRTOS 构建产物（AICP_SKIP_FREERTOS_BUILD=1）"
else
  echo "[ai-rtos] 构建 FreeRTOS-Kernel + FreeRTOS+TCP + legacy virtio-mmio/virtio-net Guest"
  FREERTOS_BUILD_DIR="${freertos_build_dir}" \
    "${repo_root}/scripts/ai-rtos/build_freertos_aicp_guest.sh"
fi

if [[ ! -s "${freertos_bin}" || ! -s "${freertos_elf}" ]]; then
  echo "ERROR: FreeRTOS 构建产物缺失" >&2
  exit 1
fi
cross_prefix="$(aicp_resolve_or_install_aarch64_none_elf \
  "${repo_root}" "14.3.rel1" CROSS_COMPILE)"
entry_point="$("${cross_prefix}readelf" -h "${freertos_elf}" | awk '/Entry point address:/ {print $4}')"
if [[ "${entry_point}" != "0xd0001000" ]]; then
  echo "ERROR: FreeRTOS ELF 入口变化，预期 0xd0001000，实际 ${entry_point}" >&2
  exit 1
fi

rm -rf "${initramfs_dir}"
mkdir -p "${initramfs_dir}"
echo "[ai-rtos] 构建 Linux Guest 静态 AICP 客户端"
rm -f "${demo_dir}/build/aarch64/aicp_init"
make -C "${demo_dir}" linux-init-aarch64 \
  CFLAGS="-O2 -g -Wall -Wextra -Werror -std=c11 -DAICP_INIT_ITERATIONS=${iterations}u -DAICP_INIT_MODE=\\\"${mode}\\\" -DAICP_INIT_SERVER=\\\"10.0.3.2\\\" -DAICP_INIT_SERVER_PORT=8800u -DAICP_INIT_CLIENT=\\\"10.0.3.3\\\" -DAICP_INIT_NET_PREFIX=\\\"10.0.3.0\\\" -DAICP_INIT_NETMASK=\\\"255.255.255.0\\\" -DAICP_INIT_SERVER_MAC=\\\"52:54:00:aa:03:02\\\" -DAICP_INIT_STATIC_ARP=1 -DAICP_INIT_STRESS_PROCS=${stress_procs}u"
cp "${demo_dir}/build/aarch64/aicp_init" "${initramfs_dir}/init"
chmod +x "${initramfs_dir}/init"
(
  cd "${initramfs_dir}"
  find . -print | cpio -o -H newc | gzip -9 > "${initramfs}"
)

echo "[ai-rtos] 生成 Linux Guest 和 AxVisor Host DTB"
crop_virtio_nodes "${linux_src_dts}" "${linux_dts}.devices" "virtio_mmio@a003c00"
# Guest DT 只能描述配置中实际分配的设备。逐级删除 QEMU virt 平台上未向
# Linux VM 映射的 MMIO 设备及其消费者，避免 Guest 驱动探测不存在的硬件。
remove_dts_nodes "${linux_dts}.devices" "${linux_dts}.pruned1" "gpio-keys|pl061@"
remove_dts_nodes "${linux_dts}.pruned1" "${linux_dts}.pruned2" "fw-cfg@|pl031@|flash@"
# 此场景不向 Guest 暴露 PMU、MSI/LPI 或 QEMU platform bus。
remove_dts_nodes "${linux_dts}.pruned2" "${linux_dts}" "platform-bus@|pmu|its@"
rm -f "${linux_dts}.devices" "${linux_dts}.pruned1" "${linux_dts}.pruned2"
patch_linux_guest_console "${linux_dts}"
patch_bootargs "${linux_dts}" "console=ttyAMA0 earlycon=pl011,0x9040000 rdinit=/init panic=-1 loglevel=7"
dtc -I dts -O dtb -o "${linux_dtb}" "${linux_dts}"
build_host_dtb
write_linux_vm_config
write_freertos_vm_config

if ! grep -Fq '[0x8000_0000, 0x2000_0000, 0x7, 1]' "${linux_vm}"; then
  echo "ERROR: Linux Guest RAM 必须使用 MapIdentical，避免 QEMU virtio DMA 访问固定预留区" >&2
  exit 1
fi
if ! grep -Fq '[0xD000_0000, 0x0800_0000, 0x7, 2]' "${freertos_vm}"; then
  echo "ERROR: FreeRTOS Guest RAM 必须使用固定 0xD0000000 MapReserved 身份映射区" >&2
  exit 1
fi

cp "${qemu_template}" "${qemu_config}"
if [[ "$(grep -Fc 'hubport,id=' "${qemu_config}")" -ne 2 ]] || \
   [[ "$(grep -Fc 'hubid=3' "${qemu_config}")" -ne 2 ]]; then
  echo "ERROR: Linux 与 FreeRTOS virtio-net 必须连接同一个 QEMU 二层 hub" >&2
  exit 1
fi
rm -f "${linux_console_log}"
LINUX_CONSOLE_LOG="${linux_console_log}" perl -0pi -e \
  's#  "-nographic",#  "-display",\n  "none",\n  "-monitor",\n  "none",\n  "-serial",\n  "stdio",\n  "-serial",\n  "file:$ENV{LINUX_CONSOLE_LOG}",#' \
  "${qemu_config}"
perl -0pi -e \
  's#  "-append",#  "-dtb",\n  "\${workspace}/tmp/ai-rtos/qemu-aarch64-freertos-host.dtb",\n  "-append",#' \
  "${qemu_config}"
if [[ "${qemu_trace}" == "1" ]]; then
  printf '%s\n' \
    'virtio_mmio_write_offset' \
    'virtio_mmio_guest_page' \
    'virtio_mmio_queue_write' \
    'virtio_mmio_setting_irq' \
    'virtio_queue_notify' \
    'virtqueue_pop' \
    'virtqueue_fill' \
    'virtqueue_flush' > "${trace_events}"
  TRACE_STAMP="${stamp}" perl -0pi -e \
    's#  "-append",#  "-trace",\n  "events=\${workspace}/tmp/ai-rtos/qemu-linux-freertos-virtio-trace-events,file=\${workspace}/tmp/ai-rtos/logs/axvisor-linux-freertos-virtio-$ENV{TRACE_STAMP}.trace",\n  "-append",#' \
    "${qemu_config}"
  echo "[ai-rtos] QEMU virtio trace：${trace_file}"
fi
if grep -Fq 'virtio-mmio.force-legacy=false' "${qemu_config}"; then
  echo "ERROR: Linux + FreeRTOS 场景必须保持 QEMU legacy virtio-mmio" >&2
  exit 1
fi
perl -0pi -e 's/(netdev=rtosnet[^"\n]*?)mrg_rxbuf=on/${1}mrg_rxbuf=off/' "${qemu_config}"
if ! grep -F "netdev=rtosnet" "${qemu_config}" | grep -Fq "mrg_rxbuf=off"; then
  echo "ERROR: FreeRTOS virtio-net 使用 10 字节 non-mergeable header，需要 mrg_rxbuf=off" >&2
  exit 1
fi

echo "[ai-rtos] 启动 AxVisor Linux + FreeRTOS 双 Guest；日志：${log_file}"
(
  cd "${repo_root}"
  aicp_exec_new_session cargo xtask axvisor qemu \
    --config os/axvisor/configs/board/qemu-aarch64.toml \
    --qemu-config "${qemu_config}" \
    --vmconfigs "${freertos_vm}" \
    --vmconfigs "${linux_vm}"
) > "${log_file}" 2>&1 &
qemu_pid=$!
global_deadline=$((SECONDS + boot_timeout_s))

wait_for_marker "smp: Brought up 1 node, 2 CPUs"
wait_for_marker "AICP_FREERTOS_NET_IRQ_ENABLED"
# Host and guest diagnostics share the AxVisor console and may split a UART line.
# A complete HELLO observed below proves that the listener accepted an IP/TCP
# connection, so readiness is gated by protocol progress instead of one log line.
wait_for_marker "AICP Linux guest client starting"
wait_for_marker "AICP_LINUX_TX_FRAME type=HELLO"
wait_for_marker "AICP_FREERTOS_HELLO"
wait_for_marker "AICP_FREERTOS_CONTROL"
wait_for_marker "AICP_LINUX_RX_FRAME type=3"
wait_for_marker "AICP_LINUX_DONE ok="

# Give the shutting-down Linux vCPU time to leave its task context, then verify
# that guest-private architectural timer sources did not leak into Host tasks.
sleep 1
if LC_ALL=C grep -a -Eq \
  'private IRQ arrived outside a vCPU task|Unhandled IRQ .*hwirq: HwIrq\((27|30)\)' \
  "${log_file}"; then
  echo "[ai-rtos] FAIL：Guest 架构定时器在 vCPU 退出后仍向 Host 注入私有 IRQ" >&2
  tail -n 220 "${log_file}" >&2 || true
  exit 1
fi

status_count="$(
  LC_ALL=C grep -ahoE 'AICP_LINUX_STATUS seq=[0-9]+' "${log_file}" "${linux_console_log}" \
    | sort -u \
    | wc -l \
    | tr -d '[:space:]'
)"
if LC_ALL=C grep -a -Eq \
  'AICP Linux guest (transaction failed|HELLO failed|connect giveup|UDP HELLO failed)|AICP Linux guest udp stale_sequence test failed' \
  "${log_file}" "${linux_console_log}"; then
  echo "[ai-rtos] FAIL：Linux 到 FreeRTOS 的 AICP 日志包含明确失败事件" >&2
  tail -n 240 "${log_file}" >&2 || true
  tail -n 160 "${linux_console_log}" >&2 || true
  exit 1
fi
if (( status_count < iterations )); then
  echo "[ai-rtos] FAIL：Linux 仅收到 ${status_count}/${iterations} 个唯一状态回传" >&2
  tail -n 240 "${log_file}" >&2 || true
  tail -n 160 "${linux_console_log}" >&2 || true
  exit 1
fi

{
  echo "AxVisor Linux + FreeRTOS AICP/TCP 双 Guest 实测摘要"
  echo "Linux: 2 vCPU -> pCPU1,pCPU2, IP 10.0.3.3, RAM 0x80000000/512MiB, NIC a003c00"
  echo "FreeRTOS: 1 vCPU -> pCPU0, IP 10.0.3.2, RAM 0xD0000000/128MiB, NIC a003a00 IRQ77"
  LC_ALL=C grep -a "AICP_FREERTOS_NET_IRQ_ENABLED" "${log_file}" | tail -n 1 || true
  LC_ALL=C grep -a "AICP_FREERTOS_NETWORK_EVENT state=up" "${log_file}" | tail -n 1 || true
  echo "FreeRTOS TCP 服务就绪证据："
  LC_ALL=C grep -a "AICP_FREERTOS_HELLO" "${log_file}" | head -n 1 || true
  LC_ALL=C grep -a "smp: Brought up 1 node, 2 CPUs" "${log_file}" | tail -n 1 || true
  LC_ALL=C grep -ah "AICP_LINUX_DONE" "${log_file}" "${linux_console_log}" | tail -n 1 || true
  echo "Linux 唯一状态回传：${status_count}/${iterations}"
  LC_ALL=C grep -ah "CONTROL" "${log_file}" "${linux_console_log}" | tail -n 3 || true
  echo "log=${log_file}"
  echo "linux_console_log=${linux_console_log}"
} | tee "${summary_file}"

echo "[ai-rtos] PASS：AxVisor 2-vCPU Linux + FreeRTOS AICP/TCP 闭环完成"
