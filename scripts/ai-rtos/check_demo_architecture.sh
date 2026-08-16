#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
#
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
demo_root="${repo_root}/apps/ai-rtos-demo"
failed=0
checked=0

fail() {
  echo "ERROR: $*" >&2
  failed=1
}

require_file() {
  local path="$1"
  checked=$((checked + 1))
  if [[ ! -f "${demo_root}/${path}" ]]; then
    fail "required demo file is missing: apps/ai-rtos-demo/${path}"
  fi
}

require_absent() {
  local path="$1"
  checked=$((checked + 1))
  if [[ -e "${demo_root}/${path}" ]]; then
    fail "obsolete or duplicated path must not exist: apps/ai-rtos-demo/${path}"
  fi
}

require_contains() {
  local path="$1"
  local pattern="$2"
  local description="$3"
  checked=$((checked + 1))
  if ! rg -q -- "${pattern}" "${demo_root}/${path}"; then
    fail "${description}: apps/ai-rtos-demo/${path}"
  fi
}

require_repo_contains() {
  local path="$1"
  local pattern="$2"
  local description="$3"
  checked=$((checked + 1))
  if ! rg -q -- "${pattern}" "${repo_root}/${path}"; then
    fail "${description}: ${path}"
  fi
}

require_repo_absent() {
  local path="$1"
  local pattern="$2"
  local description="$3"
  checked=$((checked + 1))
  if rg -q -- "${pattern}" "${repo_root}/${path}"; then
    fail "${description}: ${path}"
  fi
}

for path in \
  aicp/aicp.h \
  aicp/aicp_stream.h \
  aicp/aicp_datagram.h \
  aicp/aicp_posix_stream.h \
  aicp/aicp_client.c \
  rtos-core/aicp_service.c \
  rtos-core/control_loop.c \
  ai-model/control_policy.c \
  linux-client/main.c \
  linux-init/aicp_init.c \
  linux-init/starry_profile.h \
  rtthread-aicp/main.c \
  zephyr/src/main.c \
  freertos/main.c; do
  require_file "${path}"
done

for path in \
  linux-client/aicp_client.c \
  model-image \
  image-classifier \
  rt_compat_baseline; do
  require_absent "${path}"
done

require_contains linux-client/main.c 'aicp_client_session_transact_control' \
  "C client must use the shared AICP transaction core"
require_contains linux-init/aicp_init.c 'aicp_client_session_transact_control' \
  "Linux/Starry PID 1 must use the shared AICP transaction core"
require_contains Makefile '\$\(AICP_STARRY_DEFS\)' \
  "Starry userspace build must consume the generated network and transport definitions"
require_contains yolov8-onnx-cpu/src/main.cc 'aicp_client_session_transact_control' \
  "C++ YOLOv8 client must use the shared AICP transaction core"
require_contains rtthread-aicp/main.c '#include "aicp_service.h"' \
  "RT-Thread glue must use the shared RTOS service state machine"
require_contains rtthread-aicp/drv_virtio_aicp.c \
  'eth_device_linkchange\(&aicp_net_device->parent, RT_TRUE\)' \
  "RT-Thread virtio hook must publish link-up after the lwIP netif is attached"
require_contains rtthread-aicp/main.c \
  'aicp_virtio_net_publish_link_up\(\)' \
  "RT-Thread must republish link-up after lwIP registers the netif"
require_contains rtthread-aicp/main.c \
  'netdev_is_link_up\(netdev\)' \
  "RT-Thread must not start the TCP service before link-up"
require_contains rtthread-aicp/main.c 'tv_sec = 10' \
  "RT-Thread must allow a loaded Linux guest to progress from HELLO to control"
require_repo_absent apps/ai-rtos-demo/linux-init/aicp_init.c \
  'dump_iface_diag\("connected"\)' \
  "Linux must not run blocking diagnostics between HELLO and the first control request"
require_repo_contains apps/ai-rtos-demo/linux-init/aicp_init.c \
  'IFF_RUNNING' \
  "Linux guest init must wait for virtio carrier before its first blocking TCP connect"
require_repo_contains apps/ai-rtos-demo/linux-init/aicp_init.c \
  'netcfg step=WAIT_RUNNING' \
  "Linux guest init must expose bounded carrier-wait evidence in the runtime log"
require_repo_contains apps/ai-rtos-demo/linux-init/aicp_init.c \
  'dump_irq_diagnostics\(tag, strcmp\(tag, "configured"\) == 0\)' \
  "Linux completion must not repeat affinity procfs reads that can block during CPU bring-up"
require_contains zephyr/src/main.c '#include "aicp_service.h"' \
  "Zephyr glue must use the shared RTOS service state machine"
require_repo_contains apps/ai-rtos-demo/zephyr/src/virtio_mmio_legacy.c \
  'const uint16_t usable_descs = requested_size;' \
  "the Zephyr legacy transport must preserve the driver-requested descriptor capacity"
require_repo_contains apps/ai-rtos-demo/zephyr/src/virtio_mmio_legacy.c \
  'for \(uint16_t i = 0; i < usable_descs; \+\+i\)' \
  "the Zephyr legacy transport must expose only the driver-requested descriptors"
require_contains freertos/main.c '#include "aicp_service.h"' \
  "FreeRTOS glue must use the shared RTOS service state machine"
require_repo_absent apps/ai-rtos-demo/freertos/main.c \
  'count % (10U|20U)' \
  "FreeRTOS normal mode must not emit unbounded periodic diagnostics on the shared multi-guest console"
require_repo_contains os/axvisor/src/virtio_net.rs \
  'Configured VirtIO MMIO network devices connected by an internal L2 switch' \
  "AxVisor must provide an internal L2 switch for configured guest NICs"
require_repo_contains os/axvisor/src/virtio_net.rs \
  'add_dma_pollable_device' \
  "virtual guest networking must register its DMA and polling boundary"
require_repo_contains scripts/ai-rtos/run_axvisor_dual_guest_aicp.sh \
  'model = "virtio-net"' \
  "the primary AICP demo must use AxVisor virtual IP networking"
require_repo_absent scripts/ai-rtos/run_axvisor_dual_guest_aicp.sh \
  'hubport|passthrough_devices|passthrough =.*virtio' \
  "the primary AICP demo must not fall back to QEMU hub or passthrough networking"
require_repo_absent os/axvisor/src/manager.rs \
  'set_aarch64_passthrough_irq_routes_enabled\(vm_id, true\)' \
  "VM startup must not unmask passthrough IRQs before the guest driver enables them"
require_repo_contains virtualization/axvm/src/arch/aarch64/gic/physical.rs \
  'controller.forward_physical_spi\(self.irq\)' \
  "assigned AArch64 SPIs must retain latest-dev's hardware-backed forwarding"
require_repo_contains virtualization/axvm/src/arch/aarch64/vgic/mod.rs \
  'self.core.bind_assigned_spis\(\)' \
  "assigned AArch64 SPIs must be bound through the canonical VGIC lifecycle"
require_repo_contains virtualization/axvm/src/arch/aarch64/gic.rs \
  'physical_spi_target\(self.capabilities.host_version\(\), binding\)' \
  "assigned AArch64 SPIs must follow their target vCPU affinity"
require_repo_absent virtualization/axvm/src/arch/aarch64/gic/physical.rs \
  'request_irq' \
  "assigned AArch64 SPIs must not install a second current-EL host action"
require_repo_contains platforms/axplat-dyn/src/irq.rs \
  'register_aarch64_virtual_irq_injector' \
  "the platform IRQ path must expose one AArch64 guest-forwarding boundary"
require_repo_contains platforms/axplat-dyn/src/irq.rs \
  'active.forward_to_guest\(\)' \
  "a forwarded current-EL IRQ must transfer physical ownership to the guest"
require_repo_contains platforms/somehal/src/arch/aarch64/gic/v3.rs \
  'deactivate_on_drop: bool' \
  "the GIC active-IRQ guard must distinguish host and guest deactivation"
require_repo_contains virtualization/axvm/src/arch/aarch64/gic/physical.rs \
  'publish_from_current_el' \
  "assigned SPIs arriving at current EL must use the canonical VGIC route"
require_repo_absent virtualization/axvm/src/arch/aarch64/mod.rs \
  'std::thread::yield_now\(\)' \
  "trapped WFE must rely on the outer vCPU yield instead of yielding inside the bound run window"
require_repo_contains virtualization/arm_vcpu/src/architecture/vcpu.rs \
  'HCR_EL2::TWE::SET' \
  "WFE must trap so a guest cannot sleep on an event that the VMM does not virtualize"
require_repo_absent scripts/ai-rtos/run_axvisor_starry_rtos_aicp.sh \
  '^  \["/"\],$' \
  "dual-guest configs must assign explicit devices instead of claiming the host device tree root"
require_repo_contains scripts/ai-rtos/run_axvisor_starry_rtos_aicp.sh \
  'ax-driver/nvme' \
  "StarryOS userland builds must use the latest dev NVMe root-disk driver"
require_repo_absent scripts/ai-rtos/run_axvisor_starry_rtos_aicp.sh \
  'ax-driver/virtio-blk' \
  "StarryOS runners must not request the removed virtio-blk feature"
require_repo_contains apps/starry/aicp-control/build-aarch64-unknown-none-softfloat.toml \
  'ax-driver/nvme' \
  "the standalone StarryOS AICP app must use the latest dev NVMe root-disk driver"
require_repo_absent apps/starry/aicp-control/qemu-aarch64.toml \
  'virtio-blk' \
  "the standalone StarryOS AICP app must attach its root disk through NVMe"
require_repo_contains os/StarryOS/starryos/Cargo.toml \
  'qemu-aicp-native = \[.*dep:ax-log.*dep:ax-net.*ax-std/net' \
  "StarryOS native AICP must enable its logging and network dependencies explicitly"
require_repo_contains os/StarryOS/starryos/src/main.rs \
  '#\[cfg\(feature = "qemu-aicp-native"\)\]' \
  "StarryOS userland builds must not compile the native AICP client"
require_repo_contains net/ax-net/src/lib.rs \
  'pub fn set_static_arp_entry' \
  "ax-net must expose a typed static-neighbor API for isolated fixed-address guest links"
require_repo_contains os/StarryOS/starryos/src/native_aicp.rs \
  'set_static_arp_entry' \
  "StarryOS native AICP must install its declared peer MAC instead of depending on ARP support in every RTOS"
require_repo_absent os/StarryOS/starryos/src/main.rs \
  'starry-userland' \
  "StarryOS AICP entry must not reference the removed starry-userland feature"
require_repo_absent os/StarryOS/starryos/src/native_aicp.rs \
  'thread::yield_now\(\)' \
  "StarryOS native AICP delays must not depend on a scheduler wakeup before the kernel entry task starts"
require_repo_contains scripts/ai-rtos/run_axvisor_starry_rtos_aicp.sh \
  'AR_aarch64_unknown_none_softfloat' \
  "StarryOS cross builds on macOS must archive C objects with the target binutils"
require_repo_contains scripts/ai-rtos/run_axvisor_starry_rtos_aicp.sh \
  'target/aarch64-unknown-none-softfloat/release/starryos' \
  "StarryOS runners must consume the artifact emitted by the latest dev target"
require_repo_absent scripts/ai-rtos/run_axvisor_starry_rtos_aicp.sh \
  'target/aarch64-unknown-linux-musl/release/starryos' \
  "StarryOS runners must not look for the removed Linux-musl kernel target"
require_repo_contains scripts/ai-rtos/run_zephyr_periodic_baseline.sh \
  'ZEPHYR_BASELINE_TIMEOUT_SECONDS' \
  "Zephyr periodic baseline timeout must be configurable for loaded validation hosts"
require_repo_contains scripts/ai-rtos/run_full_qemu_validation.sh \
  'aicp_resolve_tool PYTHON python3' \
  "full validation must honor an explicit Python interpreter"
require_repo_absent scripts/ai-rtos/run_full_qemu_validation.sh \
  '^run_stage (python_syntax|isolation_unit) python3 ' \
  "full validation stages must not bypass the resolved Python interpreter"
for qemu_template in \
  os/axvisor/configs/qemu/qemu-aarch64-aicp-dual-net.toml \
  os/axvisor/configs/qemu/qemu-aarch64-aicp-dual-usernet.toml \
  os/axvisor/configs/qemu/qemu-aarch64-aicp-net.toml; do
  require_repo_absent "${qemu_template}" \
    'snapshot=on' \
    "AxVisor AICP QEMU templates must follow the latest persistent-rootfs contract"
done
for overlay in \
  configs/ai-rtos/qemu-aarch64-rtthread-reserved-memory-overlay.dts \
  configs/ai-rtos/qemu-aarch64-zephyr-reserved-memory-overlay.dts \
  configs/ai-rtos/qemu-aarch64-freertos-reserved-memory-overlay.dts; do
  require_repo_contains "${overlay}" \
    'linux@80000000' \
    "passthrough-network Linux memory must be reserved in the AxVisor host FDT"
done
require_repo_contains os/axvisor/configs/board/qemu-aarch64-ai-rtos.toml \
  '^features = \["rt-poll-idle"\]$' \
  "the production AI/RTOS profile must keep isolated vCPUs runnable across guest idle"
require_repo_contains os/axvisor/Cargo.toml \
  'rt-poll-idle = \["axvm/rt-poll-idle"\]' \
  "the AxVisor real-time profile must forward its idle policy to AxVM"
require_repo_contains os/axvisor/Cargo.toml \
  'rt-shared-wait-baseline = \["axvm/rt-shared-wait-baseline"\]' \
  "the exact latest-dev regression profile must forward its shared-wait baseline"
require_repo_contains os/axvisor/configs/board/qemu-aarch64-rt-shared-wait-baseline.toml \
  '^features = \["rt-shared-wait-baseline"\]$' \
  "the A/B baseline must preserve latest-dev's shared wait queue"
require_repo_contains os/axvisor/configs/board/qemu-aarch64-rt.toml \
  '^features = \["rt-poll-idle"\]$' \
  "the optimized A/B profile must avoid secondary-pCPU software-timer wake loss"
require_repo_contains virtualization/axvm/src/arch/aarch64/mod.rs \
  'let waits_for_event = idle_waits_for_event\(\);' \
  "AArch64 WFI must apply the selected blocking or polling idle policy"
require_repo_contains virtualization/axvm/src/architecture/exit.rs \
  'CpuSuspendStandby.*\{ return_value \}' \
  "PSCI standby must pass through the shared real-time idle policy"
require_repo_contains virtualization/axvm/src/runtime/vcpus.rs \
  'not\(feature = "rt-poll-idle"\)' \
  "CPU-isolated polling must not yield or wait away the sole pinned vCPU task"
require_repo_absent virtualization/axvm/src/vm/mod.rs \
  'vcpu_wait_queues:' \
  "production blocking waits must not depend on unstable per-vCPU queue ownership"
require_repo_contains virtualization/axvm/src/vm/mod.rs \
  'pub\(crate\) fn notify_vcpu\(&self, vcpu_id: usize\)' \
  "interrupt delivery must expose a target-vCPU wake boundary"
require_repo_contains virtualization/axvm/src/vm/mod.rs \
  'self.wait_queue.notify_all\(true\);' \
  "the optimized wake path must request an immediate local reschedule"
require_repo_contains virtualization/axvm/src/vm/mod.rs \
  'self.wait_queue.notify_all\(false\);' \
  "the latest-dev baseline must preserve deferred shared-queue wake semantics"
require_repo_contains virtualization/axvm/src/runtime/vcpus.rs \
  'runtime.notify_vcpu\(vcpu_id\);' \
  "queued interrupts must publish through the target-vCPU wake boundary"
require_repo_contains virtualization/axvm/src/runtime/mod.rs \
  'crate::host::task::send_ipi\(cpu_id\);' \
  "virtual-device notifications must kick the primary vCPU's physical CPU"
require_repo_contains virtualization/axvm/src/runtime/mod.rs \
  'not\(feature = "rt-poll-idle"\)' \
  "polling guests must not receive redundant host IPIs for device work"
require_repo_contains virtualization/axvm/src/arch/aarch64/gic/physical.rs \
  'controller.forward_physical_spi\(self.irq\)' \
  "assigned physical IRQs must retain latest-dev's hardware-backed VGIC route"
require_repo_absent virtualization/axvm/src/arch/aarch64/gic/physical.rs \
  'irq::request_irq' \
  "assigned physical IRQs must not install a competing host action"
require_repo_contains scripts/ai-rtos/run_axvisor_dual_guest_aicp.sh \
  'qemu-aarch64-ai-rtos.toml' \
  "the primary internal-network demo must enable the competition real-time idle policy"
require_repo_absent apps/ai-rtos-demo/linux-init/aicp_init.c \
  'AICP_LINUX_FINALIZE' \
  "the long-running Linux client must not saturate the emulated UART with diagnostic-only markers"
require_repo_contains apps/ai-rtos-demo/linux-init/aicp_init.c \
  'MSG_DONTWAIT' \
  "the Linux reliability client must not depend solely on a blocked socket timeout"
require_repo_contains apps/ai-rtos-demo/linux-init/aicp_init.c \
  'io_deadline_ns' \
  "the Linux reliability client must enforce its application-layer I/O deadline"
require_repo_contains apps/arceos/aicp-server/src/main.rs \
  'set_read_timeout\(Some\(CLIENT_IO_TIMEOUT\)\)' \
  "the ArceOS TCP service must release a stale client session after an I/O timeout"
require_repo_contains apps/arceos/aicp-server/src/main.rs \
  'set_write_timeout\(Some\(CLIENT_IO_TIMEOUT\)\)' \
  "the ArceOS TCP service must bound blocked status replies before accepting a reconnect"
require_repo_contains os/arceos/api/arceos_posix_api/src/imp/net.rs \
  'SetSocketOption::ReceiveTimeout' \
  "ArceOS SO_RCVTIMEO must reach ax-net instead of being silently ignored"
require_repo_contains os/arceos/api/arceos_posix_api/src/imp/net.rs \
  'SetSocketOption::SendTimeout' \
  "ArceOS SO_SNDTIMEO must reach ax-net instead of being silently ignored"
for runner in \
  scripts/ai-rtos/run_axvisor_linux_rtthread_aicp.sh \
  scripts/ai-rtos/run_axvisor_linux_zephyr_aicp.sh \
  scripts/ai-rtos/run_axvisor_linux_freertos_aicp.sh \
  scripts/ai-rtos/run_axvisor_starry_rtos_aicp.sh; do
  require_repo_contains "${runner}" \
    'qemu-aarch64-ai-rtos.toml' \
    "AI/RTOS runner must use the disk-passthrough AxVisor host profile"
  require_repo_contains "${runner}" \
    'guest_type = "virtualized"' \
    "multi-guest runners must isolate physical devices by explicit selection"
  require_repo_contains "${runner}" \
    'passthrough = \[' \
    "multi-guest runners must use the current structured passthrough schema"
  require_repo_absent "${runner}" \
    'vm_type|interrupt_mode|passthrough_devices|passthrough_addresses|excluded_devices|emu_devices' \
    "multi-guest runners must not emit removed AxVM configuration fields"
  require_repo_absent "${runner}" \
    'path = "/pl011@' \
    "machine-owned virtual serial ports must not be requested as physical passthrough devices"
done
require_repo_absent scripts/ai-rtos/run_axvisor_linux_zephyr_aicp.sh \
  'zephyr-venv-4\.2' \
  "the Zephyr v4.4 network runner must not default to the v4.2 baseline environment"
require_repo_absent scripts/ai-rtos/run_axvisor_starry_rtos_aicp.sh \
  'zephyr-venv-4\.2' \
  "the Starry/Zephyr network runner must not default to the v4.2 baseline environment"
require_repo_contains scripts/ai-rtos/run_axvisor_linux_freertos_aicp.sh \
  'tcp_timeout_ms="\$\{AICP_TCP_TIMEOUT_MS:-10000\}"' \
  "the slower FreeRTOS TCP stack must have an explicit bounded response timeout"
require_repo_contains scripts/ai-rtos/run_axvisor_linux_freertos_aicp.sh \
  'AICP_INIT_TCP_TIMEOUT_MS=\$\{tcp_timeout_ms\}u' \
  "the configured FreeRTOS response timeout must reach the Linux guest client"
require_repo_absent scripts/ai-rtos/run_axvisor_linux_freertos_aicp.sh \
  'wait_for_marker "AICP_FREERTOS_NET_IRQ_ENABLED"' \
  "an interleavable FreeRTOS diagnostic line must not gate protocol completion"
require_repo_contains scripts/ai-rtos/build_rtthread_aicp_guest.sh \
  'RTTHREAD_TEXT_OFFSET' \
  "RT-Thread builds must expose the boot-protocol image placement offset"
for runner in \
  scripts/ai-rtos/run_axvisor_linux_rtthread_aicp.sh \
  scripts/ai-rtos/run_axvisor_starry_rtos_aicp.sh; do
  require_repo_contains "${runner}" \
    'RTTHREAD_TEXT_OFFSET=0x0' \
    "AxVisor direct-boot RT-Thread images must link at their host-reserved RAM base"
  require_repo_contains "${runner}" \
    'entry_point = 0xC000_0000' \
    "AxVisor direct-boot RT-Thread entry must match its zero-offset image header"
done
for runner in \
  scripts/ai-rtos/run_axvisor_linux_rtthread_aicp.sh \
  scripts/ai-rtos/run_axvisor_linux_zephyr_aicp.sh; do
  require_repo_absent "${runner}" \
    '\|panic' \
    "terminal-failure detection must not match the normal Linux panic=-1 boot argument"
done
for runner in \
  scripts/ai-rtos/run_axvisor_linux_rtthread_aicp.sh \
  scripts/ai-rtos/run_axvisor_linux_zephyr_aicp.sh \
  scripts/ai-rtos/run_axvisor_linux_freertos_aicp.sh; do
  require_repo_contains "${runner}" \
    '\[0x8000_0000, 0x2000_0000, 0x7, 2\]' \
    "passthrough-network Linux memory must use the host-reserved identity mapping"
  require_repo_absent "${runner}" \
    'patch_linux_guest_console|0x9040000' \
    "Linux must use the AArch64 machine profile virtual PL011 at 0x09000000"
done

checked=$((checked + 1))
if rg -n '__ZEPHYR__|__RTTHREAD__|AICP_FREERTOS|Starry|Linux' \
  "${demo_root}/aicp" \
  "${demo_root}/rtos-core" \
  "${demo_root}/ai-model"; then
  fail "OS-specific branches are not allowed in shared protocol, service, or model code"
fi

checked=$((checked + 1))
protocol_definitions="$(rg -l 'static inline uint16_t aicp_crc16_ccitt_update' "${demo_root}" || true)"
if [[ "${protocol_definitions}" != "${demo_root}/aicp/aicp.h" ]]; then
  fail "AICP CRC implementation must have exactly one definition in aicp/aicp.h"
fi

checked=$((checked + 1))
if find "${demo_root}" \
  \( -type d \( -name build -o -name target -o -name install -o -name third_party \) -prune \) -o \
  -type d \
  \( -name FreeRTOS-Kernel -o -name FreeRTOS-Plus-TCP -o -name rt-thread -o -name zephyrproject \) \
  -print -quit | grep -q .; then
  fail "third-party RTOS source trees must stay under tmp/, not apps/ai-rtos-demo/"
fi

if (( failed != 0 )); then
  exit 1
fi

echo "[ai-rtos] PASS: demo architecture boundaries checked (${checked} assertions)"
