#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
#
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
revision="${RTTHREAD_REVISION:-v5.2.1}"
source_dir="${RTTHREAD_SOURCE_DIR:-${repo_root}/tmp/rt-thread-${revision}}"
bsp_dir="${source_dir}/bsp/qemu-virt64-aarch64"
build_dir="${RTTHREAD_BUILD_DIR:-${repo_root}/tmp/rtthread-aicp-build}"
ram_base="${RTTHREAD_RAM_BASE:-0x40000000}"
text_offset="${RTTHREAD_TEXT_OFFSET:-0x80000}"
gic_version="${RTTHREAD_GIC_VERSION:-2}"
virtio_mmio_base="${RTTHREAD_VIRTIO_MMIO_BASE:-0x0a000000}"
virtio_max_nr="${RTTHREAD_VIRTIO_MAX_NR:-32}"
virtio_irq_base="${RTTHREAD_VIRTIO_IRQ_BASE:-48}"
venv="${repo_root}/tmp/rtthread-venv"
toolchain_version="14.3.rel1"
source "${repo_root}/scripts/ai-rtos/lib/host_tools.sh"
source "${repo_root}/scripts/ai-rtos/lib/third_party_source_guard.sh"

if [[ ! "${ram_base}" =~ ^0x[0-9a-fA-F]+$ ]]; then
  echo "ERROR: RTTHREAD_RAM_BASE must be a hexadecimal address, got '${ram_base}'" >&2
  exit 2
fi
if [[ ! "${text_offset}" =~ ^0x[0-9a-fA-F]+$ ]]; then
  echo "ERROR: RTTHREAD_TEXT_OFFSET must be a hexadecimal address, got '${text_offset}'" >&2
  exit 2
fi
if [[ "${gic_version}" != "2" && "${gic_version}" != "3" ]]; then
  echo "ERROR: RTTHREAD_GIC_VERSION must be 2 or 3, got '${gic_version}'" >&2
  exit 2
fi
if [[ ! "${virtio_mmio_base}" =~ ^0x[0-9a-fA-F]+$ ]]; then
  echo "ERROR: RTTHREAD_VIRTIO_MMIO_BASE must be hexadecimal, got '${virtio_mmio_base}'" >&2
  exit 2
fi
if ! [[ "${virtio_max_nr}" =~ ^[0-9]+$ ]] || (( virtio_max_nr < 1 || virtio_max_nr > 32 )); then
  echo "ERROR: RTTHREAD_VIRTIO_MAX_NR must be in [1, 32], got '${virtio_max_nr}'" >&2
  exit 2
fi
if ! [[ "${virtio_irq_base}" =~ ^[0-9]+$ ]] || (( virtio_irq_base < 32 || virtio_irq_base > 1020 )); then
  echo "ERROR: RTTHREAD_VIRTIO_IRQ_BASE must be in [32, 1020], got '${virtio_irq_base}'" >&2
  exit 2
fi

if [[ ! -d "${source_dir}/.git" ]]; then
  git clone --depth 1 --branch "${revision}" \
    https://github.com/RT-Thread/rt-thread.git "${source_dir}"
fi

verify_rtthread_source() {
  third_party_assert_git_source "RT-Thread" "${source_dir}" "${revision}"
}

verify_rtthread_on_exit() {
  local status=$?
  trap - EXIT
  if ! verify_rtthread_source; then
    exit 1
  fi
  exit "${status}"
}

verify_rtthread_source
trap verify_rtthread_on_exit EXIT

if [[ ! -x "${venv}/bin/scons" ]]; then
  python3 -m venv "${venv}"
  "${venv}/bin/pip" install 'scons==4.8.1' 'kconfiglib==14.1.0'
elif ! "${venv}/bin/python" -c 'import kconfiglib' >/dev/null 2>&1; then
  "${venv}/bin/pip" install 'kconfiglib==14.1.0'
fi

cross_prefix="$(aicp_resolve_or_install_aarch64_none_elf \
  "${repo_root}" "${toolchain_version}" RTTHREAD_CC_PREFIX)"

rm -rf "${build_dir}"
mkdir -p "${build_dir}"
cp -R "${bsp_dir}/." "${build_dir}/"
sed -i.bak "s|^RTT_DIR := ../../$|RTT_DIR := ${source_dir}|" "${build_dir}/Kconfig"
rm -f "${build_dir}/Kconfig.bak"
VIRTIO_MMIO_BASE="${virtio_mmio_base}" \
  VIRTIO_MAX_NR="${virtio_max_nr}" \
  VIRTIO_IRQ_BASE="${virtio_irq_base}" \
  perl -0pi -e '
    s/#define VIRTIO_MMIO_BASE\s+0x[0-9a-fA-F]+/#define VIRTIO_MMIO_BASE    $ENV{VIRTIO_MMIO_BASE}/;
    s/#define VIRTIO_MAX_NR\s+[0-9]+/#define VIRTIO_MAX_NR       $ENV{VIRTIO_MAX_NR}/;
    s/#define VIRTIO_IRQ_BASE\s+\([^\n]+\)/#define VIRTIO_IRQ_BASE     $ENV{VIRTIO_IRQ_BASE}/;
  ' "${build_dir}/drivers/virt.h"

(
  cd "${build_dir}"
  RTT_ROOT="${source_dir}" \
    RTTHREAD_RAM_BASE="${ram_base}" \
    RTTHREAD_TEXT_OFFSET="${text_offset}" \
    RTTHREAD_GIC_VERSION="${gic_version}" \
    "${venv}/bin/python" - <<'PY'
import os

import kconfiglib

kconf = kconfiglib.Kconfig("Kconfig", warn=False)
kconf.load_config(".config")
gic_version = os.environ["RTTHREAD_GIC_VERSION"]
values = {
    "ARCH_RAM_OFFSET": os.environ["RTTHREAD_RAM_BASE"],
    "ARCH_TEXT_OFFSET": os.environ["RTTHREAD_TEXT_OFFSET"],
    "BSP_USING_GICV2": "y" if gic_version == "2" else "n",
    "BSP_USING_GICV3": "y" if gic_version == "3" else "n",
    "BSP_USING_VIRTIO_NET": "y",
    "RT_USING_VIRTIO_NET": "y",
    "RT_USING_POSIX_SOCKET": "y",
    "RT_USING_SAL": "y",
    "SAL_USING_LWIP": "y",
    "RT_USING_NETDEV": "y",
    "RT_USING_LWIP": "y",
    "RT_USING_LWIP203": "y",
    "RT_LWIP_DHCP": "y",
    "RT_LWIP_TCP": "y",
    "RT_LWIP_UDP": "y",
    "RT_LWIP_ETHTHREAD_STACKSIZE": "8192",
    "RT_LWIP_TCPTHREAD_STACKSIZE": "8192",
}
for name, value in values.items():
    symbol = kconf.syms.get(name)
    if symbol is None:
        raise SystemExit(f"missing RT-Thread Kconfig symbol: {name}")
    if not symbol.set_value(value):
        raise SystemExit(f"cannot set RT-Thread Kconfig symbol: {name}={value}")
kconf.write_config(".config")
PY
  RTT_ROOT="${source_dir}" "${venv}/bin/scons" --pyconfig-silent
)

rm -f "${build_dir}/applications/"*.c "${build_dir}/applications/"*.cpp
cp "${repo_root}/apps/ai-rtos-demo/rtthread-aicp/main.c" \
  "${build_dir}/applications/main.c"
cp "${repo_root}/apps/ai-rtos-demo/aicp/aicp.h" \
  "${build_dir}/applications/aicp.h"
cp "${repo_root}/apps/ai-rtos-demo/aicp/aicp_stream.h" \
  "${build_dir}/applications/aicp_stream.h"
cp "${repo_root}/apps/ai-rtos-demo/rtos-core/aicp_service.c" \
  "${build_dir}/applications/aicp_service.c"
cp "${repo_root}/apps/ai-rtos-demo/rtos-core/aicp_service.h" \
  "${build_dir}/applications/aicp_service.h"
cp "${repo_root}/apps/ai-rtos-demo/rtos-core/control_loop.c" \
  "${build_dir}/applications/control_loop.c"
cp "${repo_root}/apps/ai-rtos-demo/rtos-core/control_loop.h" \
  "${build_dir}/applications/control_loop.h"
cp "${repo_root}/apps/ai-rtos-demo/rtthread-aicp/drv_virtio_aicp.c" \
  "${build_dir}/drivers/drv_virtio.c"

echo "[ai-rtos] 构建 RT-Thread AICP Guest revision=${revision} ram_base=${ram_base} text_offset=${text_offset} gic=v${gic_version} virtio_base=${virtio_mmio_base} virtio_count=${virtio_max_nr} virtio_irq=${virtio_irq_base}"
(
  cd "${build_dir}"
  RTT_ROOT="${source_dir}" \
    RTT_EXEC_PATH="$(dirname "${cross_prefix}gcc")" \
    RTT_CC_PREFIX="${cross_prefix}" \
    "${venv}/bin/scons" -j"$(sysctl -n hw.ncpu 2>/dev/null || echo 4)"
)

test -s "${build_dir}/rtthread.bin"
test -s "${build_dir}/rtthread.elf"
trap - EXIT
verify_rtthread_source
echo "AICP_RTTHREAD_IMAGE=${build_dir}/rtthread.bin"
echo "AICP_RTTHREAD_ELF=${build_dir}/rtthread.elf"
