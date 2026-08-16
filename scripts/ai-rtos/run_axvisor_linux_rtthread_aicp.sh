#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
#
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

usage() {
  cat <<'EOF'
用法：
  scripts/ai-rtos/run_axvisor_linux_rtthread_aicp.sh [迭代次数] [ai|fixed] [启动超时秒数]

在同一个 QEMU/AxVisor 实例中启动：
  Linux Guest：2 vCPU，默认绑定 pCPU2、pCPU3，地址 10.0.2.14
  RT-Thread Guest：1 vCPU，默认绑定 pCPU1，virtio-net + lwIP + AICP/TCP

两个 virtio-net 设备接入同一个 QEMU 二层 hub，Linux 直接访问
RT-Thread 的 10.0.2.15:8800。
主数据通道是 TCP/IP，不使用 vsock、共享内存或 HyperCall。

环境变量 AICP_AXVISOR_BOARD_CONFIG 可指定 AxVisor 构建配置，默认使用
os/axvisor/configs/board/qemu-aarch64-ai-rtos.toml。该配置不在 AxVisor
Host 挂载交给 Guest 的磁盘。

环境变量 AICP_RTTHREAD_CLIENT_PROFILE 可选：
  control       静态 C 客户端，默认值
  yolov8-rust   Rust YOLOv8n + ONNX Runtime CPU 完整模型客户端

环境变量 AICP_TCP_TIMEOUT_MS 设置 TCP 连接和收包超时，默认 3000 ms。
EOF
}

if [[ $# -gt 3 || "${1:-}" == "-h" ]]; then
  usage >&2
  exit 2
fi

iterations="${1:-40}"
mode="${2:-ai}"
boot_timeout_s="${3:-180}"
if [[ "${mode}" != "ai" && "${mode}" != "fixed" ]]; then
  usage >&2
  exit 2
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
source "${repo_root}/scripts/ai-rtos/lib/cpu_topology.sh"
source "${repo_root}/scripts/ai-rtos/lib/dtb.sh"
source "${repo_root}/scripts/ai-rtos/lib/host_tools.sh"
source "${repo_root}/scripts/ai-rtos/lib/markers.sh"
source "${repo_root}/scripts/ai-rtos/lib/process.sh"
demo_dir="${repo_root}/apps/ai-rtos-demo"
out_dir="${repo_root}/tmp/ai-rtos"
log_dir="${out_dir}/logs"
result_dir="${out_dir}/results/axvisor-linux-rtthread"
bundle_dir="${repo_root}/tmp/images/qemu-aarch64"
rtthread_build_dir="${repo_root}/tmp/rtthread-aicp-axvisor-build"
stress_procs="${AICP_STRESS_PROCS:-0}"
reliability_test="${AICP_RTTHREAD_RELIABILITY:-0}"
client_profile="${AICP_RTTHREAD_CLIENT_PROFILE:-control}"
qemu_trace="${AICP_QEMU_TRACE:-0}"
qemu_gdb_port="${AICP_QEMU_GDB_PORT:-}"
tcp_timeout_ms="${AICP_TCP_TIMEOUT_MS:-3000}"
axvisor_board_config="${AICP_AXVISOR_BOARD_CONFIG:-os/axvisor/configs/board/qemu-aarch64-ai-rtos.toml}"
mkdir -p "${out_dir}" "${log_dir}" "${result_dir}" "${demo_dir}/build/aarch64"
aicp_configure_dual_guest_cpu_topology

[[ "${axvisor_board_config}" = /* ]] || axvisor_board_config="${repo_root}/${axvisor_board_config}"

if ! [[ "${iterations}" =~ ^[0-9]+$ ]] || (( iterations < 1 )); then
  echo "ERROR: 迭代次数必须为正整数，实际为 '${iterations}'" >&2
  exit 2
fi
if ! [[ "${stress_procs}" =~ ^[0-9]+$ ]] || (( stress_procs > 16 )); then
  echo "ERROR: AICP_STRESS_PROCS 必须在 [0, 16]，实际为 '${stress_procs}'" >&2
  exit 2
fi
if [[ "${reliability_test}" != "0" && "${reliability_test}" != "1" ]]; then
  echo "ERROR: AICP_RTTHREAD_RELIABILITY 只能为 0 或 1，实际为 '${reliability_test}'" >&2
  exit 2
fi
if [[ "${client_profile}" != "control" && "${client_profile}" != "yolov8-rust" ]]; then
  echo "ERROR: AICP_RTTHREAD_CLIENT_PROFILE 只能为 control 或 yolov8-rust" >&2
  exit 2
fi
if [[ "${qemu_trace}" != "0" && "${qemu_trace}" != "1" ]]; then
  echo "ERROR: AICP_QEMU_TRACE must be 0 or 1, got '${qemu_trace}'" >&2
  exit 2
fi
if [[ -n "${qemu_gdb_port}" ]] &&
   { ! [[ "${qemu_gdb_port}" =~ ^[0-9]+$ ]] ||
     (( qemu_gdb_port < 1024 || qemu_gdb_port > 65535 )); }; then
  echo "ERROR: AICP_QEMU_GDB_PORT must be an integer in [1024, 65535]" >&2
  exit 2
fi
if ! [[ "${tcp_timeout_ms}" =~ ^[0-9]+$ ]] ||
   ((tcp_timeout_ms < 1 || tcp_timeout_ms > 60000)); then
  echo "ERROR: AICP_TCP_TIMEOUT_MS 必须在 [1, 60000]，实际为 '${tcp_timeout_ms}'" >&2
  exit 2
fi
if [[ "${client_profile}" == "yolov8-rust" && "${reliability_test}" == "1" ]]; then
  echo "ERROR: YOLOv8 profile 不支持 Linux C 客户端的可靠性注入模式" >&2
  exit 2
fi
if [[ ! -f "${axvisor_board_config}" ]]; then
  echo "ERROR: AxVisor 构建配置不存在：${axvisor_board_config}" >&2
  exit 1
fi

stamp="$(date +%Y%m%d-%H%M%S)"
log_file="${log_dir}/axvisor-linux-rtthread-${client_profile}-aicp-${stamp}.log"
linux_console_log="${log_dir}/linux-rtthread-console-${stamp}.log"
trace_file="${log_dir}/axvisor-linux-rtthread-virtio-${stamp}.trace"
trace_events="${out_dir}/qemu-linux-rtthread-virtio-trace-events"
qemu_template="${repo_root}/os/axvisor/configs/qemu/qemu-aarch64-aicp-dual-net.toml"
qemu_config="${out_dir}/qemu-aarch64-linux-rtthread.toml"
linux_src_dts="${repo_root}/os/axvisor/configs/vms/qemu/aarch64/linux-smp1.dts"
linux_dts="${out_dir}/linux-rtthread-client.dts"
linux_dtb="${out_dir}/linux-rtthread-client.dtb"
rtthread_dts="${out_dir}/rtthread-aicp.dts"
rtthread_dtb="${out_dir}/rtthread-aicp.dtb"
host_base_dtb="${out_dir}/qemu-aarch64-rtthread-host-base.dtb"
host_overlay_dts="${repo_root}/configs/ai-rtos/qemu-aarch64-rtthread-reserved-memory-overlay.dts"
host_overlay_dtbo="${out_dir}/qemu-aarch64-rtthread-reserved-memory.dtbo"
host_dtb="${out_dir}/qemu-aarch64-rtthread-host.dtb"
host_dtb_dummy_disk="${out_dir}/qemu-dtb-dummy.img"
linux_vm="${out_dir}/linux-rtthread-client.generated.toml"
rtthread_vm="${out_dir}/rtthread-aicp.generated.toml"
initramfs_dir="${out_dir}/initramfs-linux-rtthread"
initramfs="${out_dir}/linux-rtthread-initramfs.cpio.gz"
linux_kernel="${bundle_dir}/linux/linux-qemu"
rtthread_bin="${rtthread_build_dir}/rtthread.bin"
summary_file="${result_dir}/latest-summary.txt"
yolo_demo_dir="${demo_dir}/yolov8-rust-onnx"
yolo_install_dir="${yolo_demo_dir}/install/aarch64"

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
    if aicp_logs_have_terminal_failure "${log_file}" "${linux_console_log}"; then
      echo "[ai-rtos] FAIL：检测到 AICP 终止失败汇总，停止等待：${marker}" >&2
      aicp_print_log_tails 220 "${log_file}" "${linux_console_log}"
      return 1
    fi
    if grep -a -Eq \
      'VM\[[0-9]+\] setup failed|Failed to initialize guest VM|Stopping VM\[[0-9]+\]: Fault|run VCpu\[[0-9]+\] get error|emu_device mmio (read|write) failed|Kernel panic|thread .* panicked at|Panic:' \
      "${log_file}" 2>/dev/null; then
      echo "[ai-rtos] FAIL：检测到 AxVisor Host/vCPU 异常，停止等待：${marker}" >&2
      tail -n 220 "${log_file}" >&2 || true
      tail -n 120 "${linux_console_log}" >&2 || true
      return 1
    fi
    if ! kill -0 "${qemu_pid}" 2>/dev/null; then
      echo "[ai-rtos] AxVisor 在出现标记前退出：${marker}" >&2
      tail -n 200 "${log_file}" >&2 || true
      return 1
    fi
    sleep 1
  done
  echo "[ai-rtos] 等待标记超时：${marker}" >&2
  tail -n 200 "${log_file}" >&2 || true
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

  local linux_reserved_reg reserved_reg
  linux_reserved_reg="$(fdtget -tx "${host_dtb}" /reserved-memory/linux@80000000 reg)"
  if [[ "${linux_reserved_reg}" != "0 80000000 0 20000000" ]]; then
    echo "ERROR: 宿主 DTB 中 Linux 预留内存校验失败：${linux_reserved_reg}" >&2
    exit 1
  fi
  reserved_reg="$(fdtget -tx "${host_dtb}" /reserved-memory/rtthread@c0000000 reg)"
  if [[ "${reserved_reg}" != "0 c0000000 0 10000000" ]]; then
    echo "ERROR: 宿主 DTB 中 RT-Thread 预留内存校验失败：${reserved_reg}" >&2
    exit 1
  fi
  echo "[ai-rtos] 宿主 DTB 已预留 Linux/RT-Thread DMA 内存：${linux_reserved_reg}; ${reserved_reg}"
}

write_linux_vm_config() {
  cat > "${linux_vm}" <<EOF
[base]
id = 1
name = "linux-ai-rtthread-qemu"
guest_type = "virtualized"
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
  [0x8000_0000, 0x2000_0000, 0x7, 2],
]

[devices]
passthrough = [
  { path = "/virtio_mmio@a003c00" },
]
disabled = []
EOF
}

write_rtthread_vm_config() {
  cat > "${rtthread_vm}" <<EOF
[base]
id = 2
name = "rtthread-aicp-qemu"
guest_type = "virtualized"
cpu_num = 1
phys_cpu_ids = [${rtos_vcpu0_pcpu}]
phys_cpu_sets = [${rtos_vcpu0_mask}]

[kernel]
entry_point = 0xC000_0000
image_location = "memory"
kernel_path = "${rtthread_bin}"
kernel_load_addr = 0xC000_0000
dtb_path = "${rtthread_dtb}"
dtb_load_addr = 0xCFE0_0000
memory_regions = [
  # RT-Thread virtio 驱动把 GPA 写入 virtqueue 描述符；QEMU 直通设备不会
  # 执行 AxVisor Stage-2 翻译，因此 RAM 必须使用宿主 FDT 预留的身份映射区。
  [0xC000_0000, 0x1000_0000, 0x7, 2],
]

[devices]
passthrough = [
  { path = "/virtio_mmio@a003a00" },
]
disabled = []
EOF
}

if [[ ! -f "${linux_kernel}" ]]; then
  echo "ERROR: 缺少 Linux kernel：${linux_kernel}" >&2
  echo "请先执行：cargo xtask image pull qemu-aarch64 -o tmp/images" >&2
  exit 1
fi

echo "[ai-rtos] 构建 AxVisor 专用 RT-Thread：GICv3、RAM 0xC0000000、专属 virtio 槽位"
RTTHREAD_GIC_VERSION=3 \
RTTHREAD_RAM_BASE=0xC0000000 \
RTTHREAD_TEXT_OFFSET=0x0 \
RTTHREAD_VIRTIO_MMIO_BASE=0x0a003a00 \
RTTHREAD_VIRTIO_MAX_NR=1 \
RTTHREAD_VIRTIO_IRQ_BASE=77 \
RTTHREAD_BUILD_DIR="${rtthread_build_dir}" \
  "${repo_root}/scripts/ai-rtos/build_rtthread_aicp_guest.sh"

rm -rf "${initramfs_dir}"
mkdir -p "${initramfs_dir}"
if [[ "${client_profile}" == "control" ]]; then
  echo "[ai-rtos] 构建 Linux Guest 的静态 C /init"
  rm -f "${demo_dir}/build/aarch64/aicp_init"
  make -C "${demo_dir}" linux-init-aarch64 \
    CFLAGS="-O2 -g -Wall -Wextra -Werror -std=c11 -DAICP_INIT_ITERATIONS=${iterations}u -DAICP_INIT_MODE=\\\"${mode}\\\" -DAICP_INIT_SERVER=\\\"10.0.2.15\\\" -DAICP_INIT_SERVER_PORT=8800u -DAICP_INIT_CLIENT=\\\"10.0.2.14\\\" -DAICP_INIT_NET_PREFIX=\\\"10.0.2.0\\\" -DAICP_INIT_NETMASK=\\\"255.255.255.0\\\" -DAICP_INIT_STATIC_ARP=1 -DAICP_INIT_STRESS_PROCS=${stress_procs}u -DAICP_INIT_RELIABILITY_TEST=${reliability_test} -DAICP_INIT_TCP_TIMEOUT_MS=${tcp_timeout_ms}u"
  cp "${demo_dir}/build/aarch64/aicp_init" "${initramfs_dir}/init"
else
  yolo_bundle_ready=0
  if aicp_yolo_rust_bundle_ready "${yolo_install_dir}"; then
    yolo_bundle_ready=1
  fi
  if [[ "${AICP_YOLO_RUST_SKIP_BUILD:-0}" != "1" ]] &&
     { [[ "${AICP_YOLO_RUST_REBUILD:-0}" == "1" ]] || [[ "${yolo_bundle_ready}" != "1" ]]; }; then
    echo "[ai-rtos] 使用 Docker 构建 Rust YOLOv8 ONNX Runtime CPU aarch64 包"
    AICP_TGOSKITS_ROOT="${repo_root}" "${yolo_demo_dir}/build-aarch64-docker.sh"
  elif [[ "${yolo_bundle_ready}" == "1" ]]; then
    echo "[ai-rtos] 复用完整的 Rust YOLOv8 aarch64 构建包；设置 AICP_YOLO_RUST_REBUILD=1 可强制重建"
  fi
  if [[ ! -x "${yolo_install_dir}/aicp_yolov8_rust_onnx" ]]; then
    echo "ERROR: 缺少 Rust YOLOv8 aarch64 程序：${yolo_install_dir}/aicp_yolov8_rust_onnx" >&2
    exit 1
  fi
  echo "[ai-rtos] 构建 YOLOv8 PID1 启动器并打包模型、图片和运行库"
  rm -f "${demo_dir}/build/aarch64/aicp_yolov8_init"
  make -C "${demo_dir}" yolov8-init-aarch64
  cp "${demo_dir}/build/aarch64/aicp_yolov8_init" "${initramfs_dir}/init"
  mkdir -p "${initramfs_dir}/bin"
  cp "${yolo_install_dir}/aicp_yolov8_rust_onnx" "${initramfs_dir}/bin/"
  cp -a "${yolo_install_dir}/lib" "${initramfs_dir}/lib"
  cp -a "${yolo_install_dir}/model" "${initramfs_dir}/model"
  cp -a "${yolo_install_dir}/validation" "${initramfs_dir}/validation"
fi
chmod +x "${initramfs_dir}/init"
(
  cd "${initramfs_dir}"
  find . -print | cpio -o -H newc | gzip -9 > "${initramfs}"
)

echo "[ai-rtos] 生成 Linux 与 RT-Thread 专用 DTB"
crop_virtio_nodes "${linux_src_dts}" "${linux_dts}.devices" "virtio_mmio@a003c00"
remove_dts_nodes "${linux_dts}.devices" "${linux_dts}.pruned1" "gpio-keys|pl061@"
remove_dts_nodes "${linux_dts}.pruned1" "${linux_dts}.pruned2" "fw-cfg@|pl031@|flash@"
remove_dts_nodes "${linux_dts}.pruned2" "${linux_dts}" "platform-bus@|pmu|its@"
rm -f "${linux_dts}.devices" "${linux_dts}.pruned1" "${linux_dts}.pruned2"
patch_bootargs "${linux_dts}" "console=ttyAMA0 earlycon=pl011,0x9000000 rdinit=/init panic=-1 loglevel=7"
crop_virtio_nodes "${linux_src_dts}" "${rtthread_dts}" "virtio_mmio@a003a00"
perl -0pi -e \
  's/memory\@80000000/memory\@c0000000/; s/<0x00 0x80000000 0x00 0x40000000>/<0x00 0xc0000000 0x00 0x10000000>/' \
  "${rtthread_dts}"
patch_bootargs "${rtthread_dts}" ""
dtc -I dts -O dtb -o "${linux_dtb}" "${linux_dts}"
dtc -I dts -O dtb -o "${rtthread_dtb}" "${rtthread_dts}"
build_host_dtb

write_linux_vm_config
write_rtthread_vm_config
cp "${qemu_template}" "${qemu_config}"
if [[ "$(grep -Fc 'hubport,id=' "${qemu_config}")" -ne 2 ]] || \
   [[ "$(grep -Fc 'hubid=3' "${qemu_config}")" -ne 2 ]]; then
  echo "ERROR: Linux 与 RT-Thread virtio-net 必须连接同一个 QEMU 二层 hub" >&2
  exit 1
fi
if [[ -n "${qemu_gdb_port}" ]]; then
  QEMU_GDB_PORT="${qemu_gdb_port}" perl -0pi -e \
    's#  "-cpu",#  "-gdb",\n  "tcp::$ENV{QEMU_GDB_PORT}",\n  "-cpu",#' \
    "${qemu_config}"
  echo "[ai-rtos] QEMU GDB stub: tcp::${qemu_gdb_port}"
fi
rm -f "${linux_console_log}"
LINUX_CONSOLE_LOG="${linux_console_log}" perl -0pi -e \
  's#  "-nographic",#  "-display",\n  "none",\n  "-monitor",\n  "none",\n  "-serial",\n  "stdio",\n  "-serial",\n  "file:$ENV{LINUX_CONSOLE_LOG}",#' \
  "${qemu_config}"
perl -0pi -e \
  's#  "-append",#  "-dtb",\n  "\${workspace}/tmp/ai-rtos/qemu-aarch64-rtthread-host.dtb",\n  "-append",#' \
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
    's#  "-append",#  "-trace",\n  "events=\${workspace}/tmp/ai-rtos/qemu-linux-rtthread-virtio-trace-events,file=\${workspace}/tmp/ai-rtos/logs/axvisor-linux-rtthread-virtio-$ENV{TRACE_STAMP}.trace",\n  "-append",#' \
    "${qemu_config}"
  echo "[ai-rtos] QEMU virtio trace: ${trace_file}"
fi
if ! grep -F "netdev=rtosnet" "${qemu_config}" | grep -Fq "mrg_rxbuf=on"; then
  echo "ERROR: RT-Thread virtio-net 必须显式启用 mrg_rxbuf=on，以匹配 12 字节 virtio_net_hdr" >&2
  exit 1
fi
if ! grep -F "netdev=linuxnet" "${qemu_config}" | grep -Fq "mrg_rxbuf=off"; then
  echo "ERROR: Linux virtio-net 配置缺少预期的 mrg_rxbuf=off" >&2
  exit 1
fi

echo "[ai-rtos] 启动 AxVisor Linux + RT-Thread 双 Guest；日志：${log_file}"
(
  cd "${repo_root}"
  aicp_exec_new_session cargo xtask axvisor qemu \
    --config "${axvisor_board_config}" \
    --qemu-config "${qemu_config}" \
    --vmconfigs "${rtthread_vm}" \
    --vmconfigs "${linux_vm}"
) > "${log_file}" 2>&1 &
qemu_pid=$!
global_deadline=$((SECONDS + boot_timeout_s))

wait_for_marker "AICP_RTTHREAD_READY"
wait_for_marker "smp: Brought up 1 node, 2 CPUs"
if [[ "${client_profile}" == "control" ]]; then
  wait_for_marker "AICP Linux guest client starting"
  wait_for_marker "AICP_RTTHREAD_CLIENT_CONNECTED"
  wait_for_marker "AICP_RTTHREAD_CONTROL"
  if [[ "${reliability_test}" == "1" ]]; then
    wait_for_marker "AICP_RTTHREAD_RELIABILITY_SUMMARY passed=8 failed=0"
  fi
  wait_for_marker "AICP_LINUX_DONE ok="

  status_count="$(
    LC_ALL=C grep -ahoE 'AICP_LINUX_STATUS seq=[0-9]+' "${log_file}" "${linux_console_log}" \
      | sort -u \
      | wc -l \
      | tr -d '[:space:]'
  )"
  if LC_ALL=C grep -a -Eq \
    'AICP Linux guest (transaction failed|HELLO failed|connect giveup|UDP HELLO failed)|AICP Linux guest udp stale_sequence test failed' \
    "${log_file}" "${linux_console_log}"; then
    echo "[ai-rtos] FAIL：Linux AICP 客户端日志包含明确失败事件" >&2
    tail -n 200 "${log_file}" >&2 || true
    exit 1
  fi
  if (( status_count < iterations )); then
    echo "[ai-rtos] FAIL：Linux 仅收到 ${status_count}/${iterations} 个唯一状态回传" >&2
    tail -n 200 "${log_file}" >&2 || true
    exit 1
  fi
  if [[ "${reliability_test}" == "1" ]] &&
     ! grep -q "AICP_RTTHREAD_RELIABILITY_SUMMARY passed=8 failed=0" "${log_file}" "${linux_console_log}"; then
    echo "[ai-rtos] FAIL：RT-Thread AICP 可靠性用例未全部通过" >&2
    tail -n 240 "${log_file}" >&2 || true
    exit 1
  fi
else
  wait_for_marker "AICP_YOLO_RTTHREAD_LAUNCH"
  wait_for_marker "AICP_YOLO_RUST_BEGIN"
  wait_for_marker "AICP_RTTHREAD_CLIENT_CONNECTED"
  wait_for_marker "AICP_RTTHREAD_CONTROL"
  wait_for_marker "AICP_YOLO_RUST_DONE ok="
  if ! grep -q "AICP_YOLO_RUST_DONE ok=3 failed=0" "${log_file}" "${linux_console_log}"; then
    echo "[ai-rtos] FAIL：Rust YOLOv8 未完成 3/3 图片闭环" >&2
    tail -n 240 "${log_file}" >&2 || true
    exit 1
  fi
  if [[ "$(grep -hc 'AICP_YOLO_RUST_RESULT' "${log_file}" "${linux_console_log}" | awk '{ total += $1 } END { print total + 0 }')" -lt 3 ||
        "$(grep -hc 'AICP_YOLO_RUST_CONTROL' "${log_file}" "${linux_console_log}" | awk '{ total += $1 } END { print total + 0 }')" -lt 3 ]]; then
    echo "[ai-rtos] FAIL：缺少完整的 YOLOv8 推理或 RT-Thread 状态回传记录" >&2
    tail -n 240 "${log_file}" >&2 || true
    exit 1
  fi
fi

{
  echo "AxVisor Linux + RT-Thread AICP/TCP 双 Guest 实测摘要"
  echo "Linux: 2 vCPU -> pCPU1,pCPU2, RAM 0x80000000/512MiB, NIC a003c00"
  echo "RT-Thread: 1 vCPU -> pCPU0, RAM 0xC0000000/256MiB, NIC a003a00 IRQ77"
  grep -h "smp: Brought up 1 node, 2 CPUs" "${log_file}" "${linux_console_log}" | tail -n 1
  grep "AICP_RTTHREAD_NET_UP" "${log_file}" | tail -n 1 || true
  grep "AICP_RTTHREAD_READY" "${log_file}" | tail -n 1
  if [[ "${client_profile}" == "yolov8-rust" ]]; then
    grep -h "AICP_YOLO_RUST_RESULT" "${log_file}" "${linux_console_log}"
    grep -h "AICP_YOLO_RUST_CONTROL" "${log_file}" "${linux_console_log}"
    grep -h "AICP_YOLO_RUST_DONE" "${log_file}" "${linux_console_log}" | tail -n 1
  elif [[ "${reliability_test}" == "1" ]]; then
    grep -h "AICP_RTTHREAD_RELIABILITY name=" \
      "${log_file}" "${linux_console_log}"
    grep -h "AICP_RTTHREAD_RELIABILITY_SUMMARY" \
      "${log_file}" "${linux_console_log}" | tail -n 1
    grep "AICP_RTTHREAD_DUPLICATE" "${log_file}" | tail -n 1
    grep "AICP_RTTHREAD_STALE" "${log_file}" | tail -n 1
  fi
  grep -h "AICP_LINUX_DONE" "${log_file}" "${linux_console_log}" | tail -n 1 || true
  grep "AICP_RTTHREAD_CONTROL" "${log_file}" | tail -n 3
  echo "log=${log_file}"
  echo "linux_console_log=${linux_console_log}"
} | tee "${summary_file}"

echo "[ai-rtos] PASS：AxVisor 2-vCPU Linux + RT-Thread ${client_profile} AICP/TCP 闭环完成"
