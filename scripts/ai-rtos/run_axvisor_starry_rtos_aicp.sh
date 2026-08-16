#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
#
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  scripts/ai-rtos/run_axvisor_starry_rtos_aicp.sh [iterations] [ai|fixed] [boot_timeout_seconds]

Boots two AxVisor guests in one QEMU AArch64 run:
  StarryOS AI guest, 2 vCPUs pinned to pCPU2,pCPU3 by default
  ArceOS/FreeRTOS/RT-Thread/Zephyr guest, 1 vCPU pinned to pCPU1 by default

Select the server guest with AICP_RTOS_GUEST. Native RTOS guests use AICP/TCP
over an isolated virtio-mmio hub. ArceOS also supports the UDP comparison path.
vsock/shared memory/hypercall are not used as the main data path.
EOF
}

if [[ $# -gt 3 || "${1:-}" == "-h" ]]; then
  usage >&2
  exit 2
fi

iterations="${1:-40}"
mode="${2:-ai}"
boot_timeout_s="${3:-180}"
connect_retries="${AICP_STARRY_CONNECT_RETRIES:-120}"
qemu_net_backend="${AICP_QEMU_NET_BACKEND:-hub}"
starry_native="${AICP_STARRY_NATIVE:-0}"
starry_transport="${AICP_STARRY_TRANSPORT:-udp}"
qemu_trace="${AICP_STARRY_QEMU_TRACE:-0}"
starry_transport_label="UDP"
rtos_guest="${AICP_RTOS_GUEST:-arceos}"
if [[ "${mode}" != "ai" && "${mode}" != "fixed" ]]; then
  usage >&2
  exit 2
fi
if [[ "${qemu_net_backend}" != "mcast" && "${qemu_net_backend}" != "hub" ]]; then
  echo "[ai-rtos] ERROR: AICP_QEMU_NET_BACKEND must be 'mcast' or 'hub'" >&2
  exit 2
fi
if [[ "${starry_transport}" != "tcp" && "${starry_transport}" != "udp" ]]; then
  echo "[ai-rtos] ERROR: AICP_STARRY_TRANSPORT must be 'tcp' or 'udp'" >&2
  exit 2
fi
if [[ "${starry_transport}" == "tcp" ]]; then
  starry_transport_label="TCP"
fi
case "${rtos_guest}" in
  arceos|freertos|rtthread|zephyr) ;;
  *)
    echo "[ai-rtos] ERROR: AICP_RTOS_GUEST must be arceos, freertos, rtthread, or zephyr" >&2
    exit 2
    ;;
esac
if [[ "${rtos_guest}" != "arceos" && "${starry_transport}" != "tcp" ]]; then
  echo "[ai-rtos] ERROR: ${rtos_guest} AICP server requires AICP_STARRY_TRANSPORT=tcp" >&2
  exit 2
fi
if [[ "${rtos_guest}" != "arceos" && "${starry_native}" != "1" ]]; then
  echo "[ai-rtos] ERROR: native RTOS combinations require AICP_STARRY_NATIVE=1" >&2
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
mkdir -p "${out_dir}" "${log_dir}"
aicp_configure_dual_guest_cpu_topology
stamp="$(date +%Y%m%d-%H%M%S)"
log_file="${log_dir}/axvisor-starry-${rtos_guest}-aicp-${stamp}.log"
rootfs_log="${log_dir}/starry-rootfs-${stamp}.log"
starry_app_dir="${repo_root}/apps/starry/aicp-control"
base_qemu_config="${repo_root}/os/axvisor/configs/qemu/qemu-aarch64-aicp-dual-net.toml"
qemu_config="${out_dir}/qemu-aarch64-aicp-starry-dual.generated.toml"
linux_src_dts="${repo_root}/os/axvisor/configs/vms/qemu/aarch64/linux-smp1.dts"
starry_dts="${out_dir}/starry-aicp-dual.dts"
starry_dtb="${out_dir}/starry-aicp-dual.dtb"
rtos_dts="${out_dir}/arceos-aicp-starry-dual.dts"
rtos_dtb="${out_dir}/arceos-aicp-starry-dual.dtb"
starry_vm="${out_dir}/starry-aicp-dual.generated.toml"
rtos_vm="${out_dir}/${rtos_guest}-aicp-starry-dual.generated.toml"
starry_build_config="${out_dir}/starry-aicp-build-aarch64.generated.toml"
rtos_build_config="${out_dir}/arceos-aicp-build-aarch64.generated.toml"
rootfs_img="${out_dir}/rootfs-aarch64-aicp-starry.img"
overlay_dir="${out_dir}/starry-aicp-overlay"
starry_elf="${repo_root}/target/aarch64-unknown-none-softfloat/release/starryos"
starry_bin="${starry_elf}.bin"
arceos_bin="${repo_root}/target/aarch64-unknown-linux-musl/release/arceos-aicp-server.bin"
axvisor_board_config="${AICP_AXVISOR_BOARD_CONFIG:-os/axvisor/configs/board/qemu-aarch64-ai-rtos.toml}"
freertos_build_dir="${FREERTOS_BUILD_DIR:-${out_dir}/build-freertos-aicp}"
rtthread_build_dir="${RTTHREAD_BUILD_DIR:-${repo_root}/tmp/rtthread-aicp-axvisor-build}"
zephyr_base="${ZEPHYR_BASE:-${repo_root}/tmp/zephyrproject/zephyr}"
zephyr_build_dir="${ZEPHYR_BUILD_DIR:-${repo_root}/tmp/zephyrproject/build-aicp-axvisor-v4.4}"
west_bin="${WEST:-west}"
cross_compile=""
rtthread_dts="${out_dir}/rtthread-starry-aicp.dts"
rtthread_dtb="${out_dir}/rtthread-starry-aicp.dtb"
host_raw_dtb="${out_dir}/qemu-aarch64-starry-${rtos_guest}-host-raw.dtb"
host_base_dts="${out_dir}/qemu-aarch64-starry-${rtos_guest}-host-base.dts"
host_base_dtb="${out_dir}/qemu-aarch64-starry-${rtos_guest}-host-base.dtb"
host_overlay_dtbo="${out_dir}/qemu-aarch64-starry-${rtos_guest}-reserved-memory.dtbo"
host_dtb="${out_dir}/qemu-aarch64-starry-${rtos_guest}-host.dtb"
host_dtb_dummy_disk="${out_dir}/qemu-starry-${rtos_guest}-dtb-dummy.img"

case "${rtos_guest}" in
  rtthread|zephyr)
    starry_ip="10.0.2.14"
    rtos_ip="10.0.2.15"
    ;;
  *)
    starry_ip="10.0.3.3"
    rtos_ip="10.0.3.2"
    ;;
esac

cleanup() {
  aicp_cleanup_process_tree "${qemu_pid:-}"
}
trap cleanup EXIT

debugfs_bin="$(aicp_resolve_tool DEBUGFS debugfs || true)"
if [[ -z "${debugfs_bin}" ]]; then
  cat >&2 <<'EOF'
[ai-rtos] ERROR: debugfs is required to inject /usr/bin/aicp_starry_init into
the StarryOS ext rootfs. Install e2fsprogs, or rerun with:

  DEBUGFS=/path/to/debugfs scripts/ai-rtos/run_axvisor_starry_rtos_aicp.sh ...
EOF
  exit 1
fi
export DEBUGFS="${debugfs_bin}"

# The lwprintf-rs build script compiles an ELF object with the musl cross
# compiler.  On macOS, cc-rs otherwise finds the host ar/ranlib pair, which
# replaces that ELF member with an empty Mach-O symbol table archive.
starry_target_ar="$(aicp_resolve_tool AICP_STARRY_TARGET_AR aarch64-linux-musl-ar)"
export AR_aarch64_unknown_none_softfloat="${starry_target_ar}"

wait_for_any_marker() {
  local description="$1"
  shift
  aicp_wait_for_any_marker "${description}" "$((SECONDS + boot_timeout_s))" \
    "${qemu_pid}" "${log_file}" 180 "$@"
}

write_debugfs_file() {
  local image="$1"
  local src="$2"
  local dst="$3"
  "${debugfs_bin}" -w -R "rm ${dst}" "${image}" >/dev/null 2>&1 || true
  "${debugfs_bin}" -w -R "write ${src} ${dst}" "${image}" >/dev/null
  "${debugfs_bin}" -w -R "sif ${dst} mode 0100755" "${image}" >/dev/null
}

prune_aicp_guest_dts() {
  local path="$1"
  remove_dts_nodes "${path}" "${path}.pruned1" "gpio-keys|pl061@"
  remove_dts_nodes "${path}.pruned1" "${path}.pruned2" "fw-cfg@|pl031@|flash@"
  remove_dts_nodes "${path}.pruned2" "${path}.pruned3" "platform-bus@|pmu|its@"
  mv "${path}.pruned3" "${path}"
  rm -f \
    "${path}.pruned1" \
    "${path}.pruned2"
}

normalize_qemu_pci_intx_map() {
  local path="$1"
  python3 - "${path}" <<'PYDTS'
import re
import sys

path = sys.argv[1]
lines = open(path).read().splitlines(keepends=True)
out = []
in_gic = False
gic_depth = 0
normalized_map = False
normalized_gic = False
for line in lines:
    if re.match(r"\s*intc@8000000\s*\{", line):
        in_gic = True
        gic_depth = 0
    if in_gic and "#address-cells" in line:
        line = re.sub(r"<0x[0-9a-fA-F]+>", "<0x00>", line, count=1)
        normalized_gic = True
    if "msi-map =" in line:
        continue
    if "interrupt-map =" in line:
        prefix, encoded = line.split("<", 1)
        cells_text, suffix = encoded.split(">", 1)
        cells = [int(value, 0) for value in cells_text.split()]
        if len(cells) % 10 != 0:
            raise SystemExit(f"unexpected QEMU PCI interrupt-map cell count: {len(cells)}")
        normalized = []
        for offset in range(0, len(cells), 10):
            entry = cells[offset : offset + 10]
            normalized.extend(entry[:5])
            normalized.extend(entry[7:])
        rendered = " ".join(f"0x{value:x}" for value in normalized)
        line = prefix + "<" + rendered + ">" + suffix
        normalized_map = True
    out.append(line)
    if in_gic:
        gic_depth += line.count("{") - line.count("}")
        if gic_depth == 0:
            in_gic = False
if not normalized_map or not normalized_gic:
    raise SystemExit("failed to normalize QEMU PCI INTx/GIC cells")
open(path, "w").write("".join(out))
PYDTS
}

enable_starry_pcie_intx() {
  local path="$1"
  python3 - "${path}" <<'PYDTS'
import sys

path = sys.argv[1]
lines = open(path).read().splitlines(keepends=True)
out = []
in_pcie = False
found = False
for line in lines:
    if line.startswith("\t// pcie@10000000 {"):
        in_pcie = True
        found = True
    if in_pcie:
        line = line.replace("\t// ", "\t", 1)
        # AxVisor forwards the PCI INTx SPIs through the software VGIC.  Do not
        # expose the physical ITS to the guest; this also makes ax-driver fall
        # back from MSI-X to the PCI interrupt-map route.
        if "msi-map =" in line:
            continue
        if line.startswith("\t};"):
            in_pcie = False
    out.append(line)
if not found or in_pcie:
    raise SystemExit("failed to enable the QEMU PCI host node for StarryOS")
open(path, "w").write("".join(out))
PYDTS
  normalize_qemu_pci_intx_map "${path}"
}

write_starry_vm_config() {
  local starry_passthrough
  starry_passthrough='[
  { path = "/pcie@10000000" },
  { path = "/virtio_mmio@a003c00" },
]'

  cat > "${starry_vm}" <<EOF
[base]
id = 1
name = "starry-ai-dual-qemu"
guest_type = "virtualized"
cpu_num = 2
phys_cpu_ids = [${linux_vcpu0_pcpu}, ${linux_vcpu1_pcpu}]
phys_cpu_sets = [${linux_vcpu0_mask}, ${linux_vcpu1_mask}]

[kernel]
entry_point = 0x8020_0000
image_location = "memory"
kernel_path = "${starry_bin}"
kernel_load_addr = 0x8020_0000
dtb_path = "${starry_dtb}"
dtb_load_addr = 0x8000_0000
memory_regions = [
  [0x8000_0000, 0x2000_0000, 0x7, 1],
]

[devices]
passthrough = ${starry_passthrough}
disabled = []
EOF
}

write_rtos_vm_config() {
  if [[ "${rtos_guest}" == "freertos" || "${rtos_guest}" == "zephyr" ]]; then
    local image entry name
    if [[ "${rtos_guest}" == "freertos" ]]; then
      image="${freertos_build_dir}/aicp-freertos.bin"
      entry="0xD000_1000"
      name="freertos-aicp-starry-qemu"
    else
      image="${zephyr_build_dir}/zephyr/zephyr.bin"
      entry="0xD000_1104"
      name="zephyr-aicp-starry-qemu"
    fi
    cat > "${rtos_vm}" <<EOF
[base]
id = 2
name = "${name}"
guest_type = "virtualized"
cpu_num = 1
phys_cpu_ids = [${rtos_vcpu0_pcpu}]
phys_cpu_sets = [${rtos_vcpu0_mask}]

[kernel]
entry_point = ${entry}
image_location = "memory"
kernel_path = "${image}"
kernel_load_addr = 0xD000_0000
memory_regions = [
  [0xD000_0000, 0x0800_0000, 0x7, 2],
]

[devices]
passthrough = [
  { path = "/virtio_mmio@a003a00" },
]
disabled = []
EOF
    return
  fi

  if [[ "${rtos_guest}" == "rtthread" ]]; then
    cat > "${rtos_vm}" <<EOF
[base]
id = 2
name = "rtthread-aicp-starry-qemu"
guest_type = "virtualized"
cpu_num = 1
phys_cpu_ids = [${rtos_vcpu0_pcpu}]
phys_cpu_sets = [${rtos_vcpu0_mask}]

[kernel]
entry_point = 0xC000_0000
image_location = "memory"
kernel_path = "${rtthread_build_dir}/rtthread.bin"
kernel_load_addr = 0xC000_0000
dtb_path = "${rtthread_dtb}"
dtb_load_addr = 0xCFE0_0000
memory_regions = [
  [0xC000_0000, 0x1000_0000, 0x7, 2],
]

[devices]
passthrough = [
  { path = "/virtio_mmio@a003a00" },
]
disabled = []
EOF
    return
  fi

  cat > "${rtos_vm}" <<EOF
[base]
id = 2
name = "arceos-aicp-starry-dual-qemu"
guest_type = "virtualized"
cpu_num = 1
phys_cpu_ids = [${rtos_vcpu0_pcpu}]
phys_cpu_sets = [${rtos_vcpu0_mask}]

[kernel]
entry_point = 0x8020_0000
image_location = "memory"
kernel_path = "${arceos_bin}"
kernel_load_addr = 0x8020_0000
dtb_path = "${rtos_dtb}"
dtb_load_addr = 0x8000_0000
memory_regions = [
  [0x8000_0000, 0x4000_0000, 0x7, 1],
]

[devices]
passthrough = [
  { path = "/virtio_mmio@a002a00" },
]
disabled = []
EOF
}

build_host_dtb() {
  local overlay reserved_path expected_reg actual_reg
  case "${rtos_guest}" in
    arceos)
      overlay=""
      ;;
    freertos)
      overlay="${repo_root}/configs/ai-rtos/qemu-aarch64-freertos-reserved-memory-overlay.dts"
      reserved_path="/reserved-memory/freertos@d0000000"
      expected_reg="0 d0000000 0 8000000"
      ;;
    zephyr)
      overlay="${repo_root}/configs/ai-rtos/qemu-aarch64-zephyr-reserved-memory-overlay.dts"
      reserved_path="/reserved-memory/zephyr@d0000000"
      expected_reg="0 d0000000 0 8000000"
      ;;
    rtthread)
      overlay="${repo_root}/configs/ai-rtos/qemu-aarch64-rtthread-reserved-memory-overlay.dts"
      reserved_path="/reserved-memory/rtthread@c0000000"
      expected_reg="0 c0000000 0 10000000"
      ;;
  esac

  local tool
  for tool in qemu-system-aarch64 dtc fdtoverlay fdtget; do
    command -v "${tool}" >/dev/null 2>&1 || {
      echo "[ai-rtos] ERROR: missing device-tree tool: ${tool}" >&2
      exit 1
    }
  done

  : > "${host_dtb_dummy_disk}"
  qemu-system-aarch64 \
    -display none \
    -monitor none \
    -serial null \
    -cpu cortex-a72 \
    -machine "virt,virtualization=on,gic-version=3,its=off,dumpdtb=${host_raw_dtb}" \
    -smp "${host_cpus}" \
    -m 8g \
    -device nvme,drive=disk0,serial=tgoskits,max_ioqpairs=64,msix_qsize=65 \
    -drive "id=disk0,if=none,format=raw,file=${host_dtb_dummy_disk}" \
    -device virtio-serial-device,max_ports=1 \
    -netdev hubport,id=linuxnet,hubid=3 \
    -device virtio-net-device,netdev=linuxnet,mac=52:54:00:aa:03:03 \
    -netdev hubport,id=rtosnet,hubid=3 \
    -device virtio-net-device,netdev=rtosnet,mac=52:54:00:aa:03:02
  dtc -I dtb -O dts -o "${host_base_dts}" "${host_raw_dtb}"
  normalize_qemu_pci_intx_map "${host_base_dts}"
  dtc -@ -I dts -O dtb -o "${host_base_dtb}" "${host_base_dts}"
  if [[ -z "${overlay}" ]]; then
    cp "${host_base_dtb}" "${host_dtb}"
    echo "[ai-rtos] Host PCI INTx map normalized for assigned NVMe"
    return
  fi
  dtc -@ -I dts -O dtb -o "${host_overlay_dtbo}" "${overlay}"
  fdtoverlay -i "${host_base_dtb}" -o "${host_dtb}" "${host_overlay_dtbo}"
  actual_reg="$(fdtget -tx "${host_dtb}" "${reserved_path}" reg)"
  if [[ "${actual_reg}" != "${expected_reg}" ]]; then
    echo "[ai-rtos] ERROR: reserved memory mismatch: ${actual_reg}" >&2
    exit 1
  fi
  echo "[ai-rtos] Host reserved memory ready: ${reserved_path} reg=${actual_reg}"
}

write_guest_build_configs() {
  local starry_features
  if [[ "${starry_native}" == "1" ]]; then
    starry_features='[
  "qemu-aicp-native",
  "smp-aicp-native",
  "ax-driver/nvme",
  "ax-driver/virtio-net",
]'
  else
    starry_features='[
  "qemu-aicp-userland",
  "smp",
  "ax-driver/nvme",
  "ax-driver/virtio-net",
]'
  fi

  cat > "${starry_build_config}" <<EOF
target = "aarch64-unknown-none-softfloat"
log = "Info"
env = { AX_IP = "${starry_ip}", AX_GW = "0.0.0.0", AX_PREFIX_LEN = "24", AX_NET_MAC = "52:54:00:aa:03:03", AX_IFACE_NAME = "eth0", AICP_STARRY_NATIVE = "${starry_native}", AICP_STARRY_ITERATIONS = "${iterations}", AICP_STARRY_MODE = "${mode}", AICP_STARRY_SERVER = "${rtos_ip}", AICP_STARRY_SERVER_PORT = "8800", AICP_STARRY_SERVER_MAC = "52:54:00:aa:03:02", AICP_STARRY_IFACE = "eth0", AICP_STARRY_TRANSPORT = "${starry_transport}", AICP_STARRY_UDP_RETRIES = "${AICP_STARRY_UDP_RETRIES:-8}", AICP_STARRY_CONNECT_RETRIES = "${connect_retries}", AX_NET_STRICT_CONFIG = "1", AX_NET_DRIVER_MAC_FILTER = "1" }
features = ${starry_features}
max_cpu_num = 2
EOF

  cat > "${rtos_build_config}" <<'EOF'
features = []
log = "Info"
env = { AX_IP = "10.0.3.2", AX_GW = "0.0.0.0", AX_PREFIX_LEN = "24", AX_NET_MAC = "52:54:00:aa:03:02", AX_IFACE_NAME = "aicp0", AX_NET_STRICT_CONFIG = "1", AX_NET_DRIVER_MAC_FILTER = "1" }
max_cpu_num = 1
EOF
}

echo "[ai-rtos] Preparing StarryOS rootfs; log: ${rootfs_log}"
write_guest_build_configs
(
  cd "${repo_root}"
  cargo xtask starry rootfs --arch aarch64
) >"${rootfs_log}" 2>&1
base_rootfs="$(sed -n 's/.*rootfs ready at //p' "${rootfs_log}" | tail -n 1)"
if [[ -z "${base_rootfs}" ]]; then
  cached_rootfs="${repo_root}/tmp/axbuild/rootfs/rootfs-aarch64-alpine.img/rootfs-aarch64-alpine.img"
  if [[ -f "${cached_rootfs}" ]]; then
    base_rootfs="${cached_rootfs}"
  fi
fi
if [[ -z "${base_rootfs}" || ! -f "${base_rootfs}" ]]; then
  echo "[ai-rtos] failed to locate prepared StarryOS rootfs" >&2
  cat "${rootfs_log}" >&2 || true
  exit 1
fi
cp "${base_rootfs}" "${rootfs_img}"

rm -rf "${overlay_dir}"
mkdir -p "${overlay_dir}"
if [[ "${starry_native}" == "1" ]]; then
  echo "[ai-rtos] StarryOS native AICP mode enabled; skipping rootfs userspace injection"
else
  STARRY_APP_DIR="${starry_app_dir}" \
  STARRY_WORKSPACE="${repo_root}" \
  STARRY_ARCH="aarch64" \
  STARRY_ROOTFS="${rootfs_img}" \
  STARRY_STAGING_ROOT="${out_dir}/starry-aicp-staging" \
  STARRY_OVERLAY_DIR="${overlay_dir}" \
  AICP_STARRY_ITERATIONS="${iterations}" \
  AICP_STARRY_MODE="${mode}" \
  AICP_STARRY_SERVER="10.0.3.2" \
  AICP_STARRY_SERVER_PORT="8800" \
  AICP_STARRY_CLIENT="10.0.3.3" \
  AICP_STARRY_NET_PREFIX="10.0.3.0" \
  AICP_STARRY_STATIC_ARP="1" \
  AICP_STARRY_SERVER_MAC="52:54:00:aa:03:02" \
  AICP_STARRY_IFACE="eth0" \
  AICP_STARRY_TRANSPORT="${starry_transport}" \
  AICP_STARRY_UDP_RETRIES="${AICP_STARRY_UDP_RETRIES:-8}" \
  AICP_STARRY_CONNECT_RETRIES="${connect_retries}" \
    "${starry_app_dir}/prebuild.sh"

  "${debugfs_bin}" -w -R "mkdir /usr" "${rootfs_img}" >/dev/null 2>&1 || true
  "${debugfs_bin}" -w -R "mkdir /usr/bin" "${rootfs_img}" >/dev/null 2>&1 || true
  write_debugfs_file "${rootfs_img}" "${overlay_dir}/usr/bin/aicp_starry_init" "/usr/bin/aicp_starry_init"
  write_debugfs_file "${rootfs_img}" "${overlay_dir}/usr/bin/aicp-starry-run.sh" "/usr/bin/aicp-starry-run.sh"
  write_debugfs_file "${rootfs_img}" "${overlay_dir}/usr/bin/starry-run-case-tests" "/usr/bin/starry-run-case-tests"
fi

python3 - \
  "${base_qemu_config}" \
  "${qemu_config}" \
  "${rootfs_img}" \
  "${qemu_net_backend}" \
  "${rtos_guest}" \
  "${host_dtb}" \
  "${qemu_trace}" \
  "${log_dir}/axvisor-starry-${rtos_guest}-qemu-${stamp}.trace" <<'PYQ'
import sys
src, dst, rootfs, net_backend, rtos_guest, host_dtb, qemu_trace, trace_file = sys.argv[1:]
text = open(src).read()
text = text.replace('file=${workspace}/tmp/axbuild/rootfs/rootfs-aarch64-alpine.img', 'file=' + rootfs)
text = text.replace(
    '"virtio-blk-device,drive=disk0",',
    '"nvme,drive=disk0,serial=tgoskits,max_ioqpairs=64,msix_qsize=65",',
    1,
)
text = text.replace(
    "virt,virtualization=on,gic-version=3",
    "virt,virtualization=on,gic-version=3,its=off",
    1,
)
lines = text.splitlines(keepends=True)
out = []
reserved_disk_slot = False
inserted_arceos_padding = False
for line in lines:
    if not reserved_disk_slot and '"-netdev"' in line:
        out.append('  "-device",\n')
        out.append('  "virtio-serial-device,max_ports=1",\n')
        reserved_disk_slot = True
    out.append(line)
    if rtos_guest == "arceos" and not inserted_arceos_padding and '"virtio-net-device,netdev=linuxnet' in line:
        for _ in range(8):
            out.append('  "-device",\n')
            out.append('  "virtio-serial-device,max_ports=1",\n')
        inserted_arceos_padding = True
if not reserved_disk_slot:
    raise SystemExit("failed to reserve the former root-disk virtio-mmio slot")
if rtos_guest == "arceos" and not inserted_arceos_padding:
    raise SystemExit("failed to insert AICP virtio-mmio padding after linuxnet")
text = ''.join(out)
if rtos_guest == "freertos":
    text = text.replace(
        "netdev=rtosnet,mac=52:54:00:aa:03:02,csum=off,guest_csum=off,guest_tso4=off,guest_tso6=off,guest_ecn=off,guest_ufo=off,host_tso4=off,host_tso6=off,host_ecn=off,host_ufo=off,mrg_rxbuf=on",
        "netdev=rtosnet,mac=52:54:00:aa:03:02,csum=off,guest_csum=off,guest_tso4=off,guest_tso6=off,guest_ecn=off,guest_ufo=off,host_tso4=off,host_tso6=off,host_ecn=off,host_ufo=off,mrg_rxbuf=off",
    )
append = '  "-append",\n'
if append not in text:
    raise SystemExit("failed to locate QEMU -append insertion point")
text = text.replace(
    append,
    '  "-dtb",\n  "' + host_dtb + '",\n' + append,
    1,
)
if qemu_trace == "1":
    trace_events = [
        "pci_nvme_irq_pin",
        "pci_nvme_irq_masked",
        "pci_nvme_mmio_intm_set",
        "pci_nvme_mmio_intm_clr",
        "pci_route_irq",
        "gic_enable_irq",
        "gic_disable_irq",
        "gicv3_dist_set_irq",
    ]
    trace_events_file = trace_file + ".events"
    open(trace_events_file, "w").write("\n".join(trace_events) + "\n")
    text = text.replace(
        append,
        '  "-trace",\n  "events=' + trace_events_file + ',file=' + trace_file + '",\n' + append,
        1,
    )
if net_backend == "mcast":
    text = text.replace(
        "hubport,id=linuxnet,hubid=3",
        "socket,id=linuxnet,mcast=230.0.3.1:8803",
    )
    text = text.replace(
        "hubport,id=rtosnet,hubid=3",
        "socket,id=rtosnet,mcast=230.0.3.1:8803",
    )
open(dst, 'w').write(text)
PYQ

echo "[ai-rtos] Building StarryOS kernel..."
(
  cd "${repo_root}"
  cargo xtask starry build --arch aarch64 --config "${starry_build_config}" --smp 2
)
if [[ ! -f "${starry_elf}" ]]; then
  echo "ERROR: StarryOS ELF not found at ${starry_elf}" >&2
  exit 1
fi
if command -v rust-objcopy >/dev/null 2>&1; then
  rust-objcopy --strip-all -O binary "${starry_elf}" "${starry_bin}"
elif command -v llvm-objcopy >/dev/null 2>&1; then
  llvm-objcopy --strip-all -O binary "${starry_elf}" "${starry_bin}"
else
  echo "ERROR: rust-objcopy or llvm-objcopy is required to create ${starry_bin}" >&2
  exit 1
fi

case "${rtos_guest}" in
  arceos)
    echo "[ai-rtos] Building ArceOS reference RTOS guest with static IP ${rtos_ip}/24..."
    (
      cd "${repo_root}"
      cargo xtask arceos build \
        -p arceos-aicp-server \
        --arch aarch64 \
        --config "${rtos_build_config}"
    )
    [[ -f "${arceos_bin}" ]] || {
      echo "ERROR: ArceOS AICP image not found at ${arceos_bin}" >&2
      exit 1
    }
    ;;
  freertos)
    echo "[ai-rtos] Building FreeRTOS-Kernel + FreeRTOS+TCP overlay guest..."
    FREERTOS_BUILD_DIR="${freertos_build_dir}" \
      "${repo_root}/scripts/ai-rtos/build_freertos_aicp_guest.sh"
    [[ -s "${freertos_build_dir}/aicp-freertos.bin" ]] || {
      echo "ERROR: FreeRTOS image is missing" >&2
      exit 1
    }
    ;;
  rtthread)
    echo "[ai-rtos] Building RT-Thread overlay guest without modifying upstream sources..."
    RTTHREAD_GIC_VERSION=3 \
    RTTHREAD_RAM_BASE=0xC0000000 \
    RTTHREAD_TEXT_OFFSET=0x0 \
    RTTHREAD_VIRTIO_MMIO_BASE=0x0a003a00 \
    RTTHREAD_VIRTIO_MAX_NR=1 \
    RTTHREAD_VIRTIO_IRQ_BASE=77 \
    RTTHREAD_BUILD_DIR="${rtthread_build_dir}" \
      "${repo_root}/scripts/ai-rtos/build_rtthread_aicp_guest.sh"
    [[ -s "${rtthread_build_dir}/rtthread.bin" ]] || {
      echo "ERROR: RT-Thread image is missing" >&2
      exit 1
    }
    ;;
  zephyr)
    echo "[ai-rtos] Building Zephyr overlay application without modifying upstream sources..."
    cross_compile="$(aicp_resolve_cross_prefix CROSS_COMPILE \
      aarch64-none-elf- \
      aarch64-elf- \
      "${repo_root}"/tmp/arm-gnu-toolchain-*/bin/aarch64-none-elf-)"
    ZEPHYR_BASE="${zephyr_base}" \
    WEST="${west_bin}" \
    ZEPHYR_BUILD_DIR="${zephyr_build_dir}" \
    ZEPHYR_TOOLCHAIN_VARIANT=cross-compile \
    CROSS_COMPILE="${cross_compile}" \
    AICP_ZEPHYR_PROFILE=axvisor-virtio \
      "${repo_root}/scripts/ai-rtos/build_zephyr_aicp_guest.sh"
    [[ -s "${zephyr_build_dir}/zephyr/zephyr.bin" ]] || {
      echo "ERROR: Zephyr image is missing" >&2
      exit 1
    }
    ;;
esac

crop_virtio_nodes "${linux_src_dts}" "${starry_dts}" "virtio_mmio@a003c00"
enable_starry_pcie_intx "${starry_dts}"
prune_aicp_guest_dts "${starry_dts}"
if [[ "${AICP_STARRY_NET_POLL_ONLY:-0}" == "1" ]]; then
python3 - "${starry_dts}" <<'PYDTS'
import re
import sys

path = sys.argv[1]
text = open(path).read()

def disable_net_irq(match):
    node = match.group(0)
    return re.sub(r'\n\t\tinterrupts = <[^;]+>;', '', node, count=1)

text = re.sub(
    r'\tvirtio_mmio@a003c00 \{.*?\n\t\};',
    disable_net_irq,
    text,
    count=1,
    flags=re.S,
)
open(path, 'w').write(text)
PYDTS
fi
if [[ "${starry_native}" != "1" ]]; then
  patch_bootargs "${starry_dts}" \
    "root=/dev/vda rw init=/usr/bin/aicp_starry_init fsck.repair=yes"
fi
if [[ "${rtos_guest}" == "arceos" ]]; then
  crop_virtio_nodes "${linux_src_dts}" "${rtos_dts}" "virtio_mmio@a002a00"
  prune_aicp_guest_dts "${rtos_dts}"
  patch_bootargs "${rtos_dts}" ""
  dtc -I dts -O dtb -o "${rtos_dtb}" "${rtos_dts}"
fi
if [[ "${rtos_guest}" == "rtthread" ]]; then
  crop_virtio_nodes "${linux_src_dts}" "${rtthread_dts}" "virtio_mmio@a003a00"
  dtc -I dts -O dtb -o "${rtthread_dtb}" "${rtthread_dts}"
fi
dtc -I dts -O dtb -o "${starry_dtb}" "${starry_dts}"
build_host_dtb
write_starry_vm_config
write_rtos_vm_config

echo "[ai-rtos] Booting AxVisor StarryOS+RTOS AICP hub; log: ${log_file}"
(
  cd "${repo_root}"
  aicp_exec_new_session cargo xtask axvisor qemu \
    --config "${axvisor_board_config}" \
    --rootfs "${rootfs_img}" \
    --qemu-config "${qemu_config}" \
    --vmconfigs "${rtos_vm}" \
    --vmconfigs "${starry_vm}"
) >"${log_file}" 2>&1 &
qemu_pid=$!

case "${rtos_guest}" in
  arceos)
    rtos_network_markers=("AICP_RTOS_READY" "AICP ArceOS RTOS TCP server listening" "AICP ArceOS RTOS UDP server listening" "AICP client connected:")
    rtos_ready_markers=("AICP_RTOS_READY" "AICP ArceOS RTOS TCP server listening" "AICP ArceOS RTOS UDP server listening")
    rtos_hello_marker="AICP HELLO"
    rtos_control_marker="CONTROL seq="
    ;;
  freertos)
    # AxVisor, StarryOS and FreeRTOS share the QEMU serial stream. Early boot
    # messages may interleave at character granularity, so accept later,
    # semantically equivalent network-up markers as well as the IRQ marker.
    rtos_network_markers=(
      "AICP_FREERTOS_NET_IRQ_ENABLED"
      "AICP_FREERTOS_NETWORK_EVENT state=up"
      "AICP_FREERTOS_READY transport=tcp port=8800"
      "AICP_FREERTOS_HELLO"
    )
    rtos_ready_markers=("AICP_FREERTOS_READY transport=tcp port=8800" "AICP_FREERTOS_HELLO")
    rtos_hello_marker="AICP_FREERTOS_HELLO"
    rtos_control_marker="AICP_FREERTOS_CONTROL"
    ;;
  rtthread)
    rtos_network_markers=("AICP_RTTHREAD_NET_UP")
    rtos_ready_markers=("AICP_RTTHREAD_READY transport=tcp port=8800" "AICP_RTTHREAD_HELLO")
    rtos_hello_marker="AICP_RTTHREAD_HELLO"
    rtos_control_marker="AICP_RTTHREAD_CONTROL"
    ;;
  zephyr)
    rtos_network_markers=("AICP_ZEPHYR_NET_UP" "AICP_ZEPHYR_HELLO")
    rtos_ready_markers=("AICP Zephyr RTOS server listening" "AICP_ZEPHYR_HELLO")
    rtos_hello_marker="AICP_ZEPHYR_HELLO"
    rtos_control_marker="AICP_ZEPHYR_CONTROL"
    ;;
esac

wait_for_any_marker "${rtos_guest} network ready" "${rtos_network_markers[@]}"
wait_for_any_marker "${rtos_guest} AICP server ready" "${rtos_ready_markers[@]}"
wait_for_any_marker "StarryOS AICP client start" \
  "AICP_STARRY_NATIVE_SPAWN" \
  "AICP_STARRY_NATIVE_START" \
  "AICP StarryOS guest client starting" \
  "AICP_STARRY_RUN starting"
if [[ "${starry_native}" == "1" && "${starry_transport}" == "tcp" ]]; then
  wait_for_any_marker "StarryOS native TCP connection" \
    "AICP_STARRY_NATIVE_TCP_CONNECTED" \
    "TCP connection from ${starry_ip}:" \
    "AICP_STARRY_NATIVE_HELLO transport=tcp"
  wait_for_any_marker "StarryOS native HELLO" \
    "AICP_STARRY_NATIVE_HELLO transport=tcp" \
    "${rtos_hello_marker}"
  wait_for_any_marker "StarryOS native STATUS" \
    "AICP_STARRY_NATIVE_STATUS" \
    "AICP_STARRY_DONE ok="
else
  wait_for_any_marker "StarryOS AICP IP connection" \
    "AICP_STARRY_NATIVE_HELLO" \
    "AICP StarryOS guest connected" \
    "AICP StarryOS_guest connected transport=udp" \
    "AICP UDP HELLO" \
    "AICP client connected:" \
    "AICP HELLO"
fi
wait_for_any_marker "StarryOS AICP completion" \
  "AICP_STARRY_DONE ok="

if ! LC_ALL=C grep -a -q "AICP_STARRY_DONE ok=.*failed=0" "${log_file}"; then
  echo "[ai-rtos] FAIL: StarryOS guest AICP client reported failures" >&2
  tail -n 180 "${log_file}" >&2 || true
  exit 1
fi
if ! LC_ALL=C grep -a -q "${rtos_control_marker}" "${log_file}"; then
  echo "[ai-rtos] FAIL: RTOS guest did not log CONTROL messages" >&2
  tail -n 180 "${log_file}" >&2 || true
  exit 1
fi

# The marker-driven success path can finish before `cargo xtask` has reaped
# QEMU. Tear down the entire launch session and verify that the writable root
# image is unlocked before another matrix stage reuses it.
aicp_cleanup_process_tree "${qemu_pid}"
qemu_pid=""
aicp_wait_for_qemu_image_release "${rootfs_img}" 20

LC_ALL=C grep -a "AICP_STARRY_DONE" "${log_file}" | tail -n 1
echo "[ai-rtos] PASS: AxVisor StarryOS+RTOS AICP ${starry_transport_label}/IP closed loop complete"
echo "[ai-rtos] log: ${log_file}"
