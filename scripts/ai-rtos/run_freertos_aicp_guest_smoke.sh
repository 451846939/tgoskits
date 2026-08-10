#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

usage() {
  cat <<'EOF'
用法：
  scripts/ai-rtos/run_freertos_aicp_guest_smoke.sh [超时秒数]

在一个 QEMU/AxVisor 实例中启动 TGOSKits 自有 FreeRTOS AArch64 Guest，
验证 EL1 启动、GICv3 虚拟定时器、FreeRTOS 调度器、legacy virtio-net、
FreeRTOS+TCP 网络上线和 AICP TCP 监听服务。
EOF
}

if [[ $# -gt 1 || "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage >&2
  exit 2
fi

timeout_s="${1:-45}"
if ! [[ "${timeout_s}" =~ ^[0-9]+$ ]] || (( timeout_s < 10 )); then
  echo "ERROR: 超时必须是大于等于 10 的整数秒" >&2
  exit 2
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
source "${repo_root}/scripts/ai-rtos/lib/markers.sh"
source "${repo_root}/scripts/ai-rtos/lib/process.sh"
out_dir="${repo_root}/tmp/ai-rtos"
log_dir="${out_dir}/logs"
build_dir="${out_dir}/build-freertos-aicp"
qemu_template="${repo_root}/os/axvisor/configs/qemu/qemu-aarch64-aicp-dual-net.toml"
qemu_config="${out_dir}/qemu-aarch64-freertos-smoke.toml"
host_base_dtb="${out_dir}/qemu-aarch64-freertos-host-base.dtb"
host_overlay_dts="${repo_root}/configs/ai-rtos/qemu-aarch64-zephyr-reserved-memory-overlay.dts"
host_overlay_dtbo="${out_dir}/qemu-aarch64-freertos-reserved-memory.dtbo"
host_dtb="${out_dir}/qemu-aarch64-freertos-host.dtb"
dummy_disk="${out_dir}/qemu-freertos-dtb-dummy.img"
vm_config="${out_dir}/freertos-aicp-smoke.generated.toml"
freertos_bin="${build_dir}/aicp-freertos.bin"
stamp="$(date +%Y%m%d-%H%M%S)"
log_file="${log_dir}/axvisor-freertos-aicp-smoke-${stamp}.log"
mkdir -p "${out_dir}" "${log_dir}"

cleanup() {
  aicp_cleanup_process_tree "${qemu_pid:-}"
}
trap cleanup EXIT INT TERM

wait_for_marker() {
  aicp_wait_for_marker "$1" "${deadline}" "${qemu_pid}" "${log_file}" 200
}

for tool in qemu-system-aarch64 dtc fdtoverlay fdtget; do
  if ! command -v "${tool}" >/dev/null 2>&1; then
    echo "ERROR: 缺少工具：${tool}" >&2
    exit 1
  fi
done

echo "[ai-rtos] 构建 FreeRTOS 原生 Guest"
"${repo_root}/scripts/ai-rtos/build_freertos_aicp_guest.sh"
if [[ ! -s "${freertos_bin}" ]]; then
  echo "ERROR: FreeRTOS Guest 镜像不存在：${freertos_bin}" >&2
  exit 1
fi

echo "[ai-rtos] 生成并校验 Host DTB 的 FreeRTOS 预留内存"
: > "${dummy_disk}"
qemu-system-aarch64 \
  -nographic \
  -cpu cortex-a72 \
  -machine "virt,virtualization=on,gic-version=3,dumpdtb=${host_base_dtb}" \
  -smp 4 \
  -m 8g \
  -device virtio-blk-device,drive=disk0 \
  -drive "id=disk0,if=none,format=raw,file=${dummy_disk}" \
  -netdev hubport,id=linuxnet,hubid=3 \
  -device virtio-net-device,netdev=linuxnet,mac=52:54:00:aa:03:03 \
  -netdev hubport,id=rtosnet,hubid=3 \
  -device virtio-net-device,netdev=rtosnet,mac=52:54:00:aa:03:02
dtc -@ -I dts -O dtb -o "${host_overlay_dtbo}" "${host_overlay_dts}"
fdtoverlay -i "${host_base_dtb}" -o "${host_dtb}" "${host_overlay_dtbo}"
reserved_reg="$(fdtget -tx "${host_dtb}" /reserved-memory/zephyr@d0000000 reg)"
if [[ "${reserved_reg}" != "0 d0000000 0 8000000" ]]; then
  echo "ERROR: Host DTB 预留区校验失败：${reserved_reg}" >&2
  exit 1
fi

cat > "${vm_config}" <<EOF
[base]
id = 2
name = "freertos-aicp-smoke-qemu"
vm_type = 1
cpu_num = 1
phys_cpu_ids = [0]

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
  ["/pl011@9000000"],
]
passthrough_addresses = []
excluded_devices = []
emu_devices = [
  ["gppt-gicd", 0x0800_0000, 0x1_0000, 0, 0x21, []],
  ["gppt-gicr", 0x080a_0000, 0x2_0000, 0, 0x20, [1, 0x2_0000, 0]],
]
EOF

cp "${qemu_template}" "${qemu_config}"
HOST_DTB="${host_dtb}" perl -0pi -e \
  's#  "-append",#  "-dtb",\n  "$ENV{HOST_DTB}",\n  "-append",#' \
  "${qemu_config}"

echo "[ai-rtos] 启动 AxVisor + FreeRTOS；日志：${log_file}"
# Build AxVisor before starting the guest timeout. This keeps a cold Rust build
# from being reported as a guest boot failure.
(
  cd "${repo_root}"
  cargo xtask axvisor build \
    --config os/axvisor/configs/board/qemu-aarch64.toml \
    --vmconfigs "${vm_config}"
)

(
  cd "${repo_root}"
  aicp_exec_new_session cargo xtask axvisor qemu \
    --config os/axvisor/configs/board/qemu-aarch64.toml \
    --qemu-config "${qemu_config}" \
    --vmconfigs "${vm_config}"
) > "${log_file}" 2>&1 &
qemu_pid=$!
deadline=$((SECONDS + timeout_s))

wait_for_marker "AICP_FREERTOS_BOOT"
wait_for_marker "AICP_FREERTOS_SCHEDULER_START"
wait_for_marker "AICP_FREERTOS_VIRTIO_READY"
wait_for_marker "AICP_FREERTOS_NETWORK_EVENT state=up"
wait_for_marker "AICP_FREERTOS_READY transport=tcp port=8800"
wait_for_marker "AICP_FREERTOS_TICK"
wait_for_marker "AICP_FREERTOS_TASK_SWITCH"

# Guest UART bytes and host logs share one QEMU console, so a host log can split
# the NET_IRQ_ENABLED line in the middle. VIRTIO_READY is emitted only after the
# same initialization function has enabled IRQ 77, and the subsequent network-up
# marker proves that the initialized interface is operational.

echo "[ai-rtos] FreeRTOS 原生 Guest 验证摘要："
grep -E "AICP_FREERTOS_(BOOT|SCHEDULER_START|VIRTIO_READY|NET_IRQ_ENABLED|NETWORK_EVENT|READY|TICK|TASK_SWITCH)" "${log_file}" | head -n 24 || true
echo "[ai-rtos] PASS：AxVisor FreeRTOS 原生 Guest 调度、virtio-net、FreeRTOS+TCP 与 AICP 服务完成"
echo "log=${log_file}"
