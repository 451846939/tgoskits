#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
#
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

app_dir="${STARRY_APP_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)}"
workspace="${STARRY_WORKSPACE:-$(cd "${app_dir}/../../.." && pwd)}"
overlay_dir="${STARRY_OVERLAY_DIR:-}"

if [[ -z "${overlay_dir}" ]]; then
    echo "error: STARRY_OVERLAY_DIR is required" >&2
    exit 1
fi

demo_dir="${workspace}/apps/ai-rtos-demo"
iterations="${AICP_STARRY_ITERATIONS:-20}"
mode="${AICP_STARRY_MODE:-ai}"
server="${AICP_STARRY_SERVER:-10.0.2.2}"
server_port="${AICP_STARRY_SERVER_PORT:-8800}"
client="${AICP_STARRY_CLIENT:-10.0.2.15}"
net_prefix="${AICP_STARRY_NET_PREFIX:-10.0.2.0}"
netmask="${AICP_STARRY_NETMASK:-255.255.255.0}"
static_arp="${AICP_STARRY_STATIC_ARP:-0}"
server_mac="${AICP_STARRY_SERVER_MAC:-52:54:00:aa:03:02}"
connect_retries="${AICP_STARRY_CONNECT_RETRIES:-80}"
transport="${AICP_STARRY_TRANSPORT:-udp}"
udp_retries="${AICP_STARRY_UDP_RETRIES:-8}"
udp_reorder_test="${AICP_STARRY_UDP_REORDER_TEST:-0}"
iface="${AICP_STARRY_IFACE:-aicp0}"

starry_defs="-DAICP_INIT_GUEST_LABEL=\\\"StarryOS_guest\\\""
starry_defs+=" -DAICP_INIT_ROLE=\\\"starryos-guest-init\\\""
starry_defs+=" -DAICP_INIT_DONE_TOKEN=\\\"AICP_STARRY_DONE\\\""
starry_defs+=" -DAICP_INIT_STATUS_TOKEN=\\\"AICP_STARRY_STATUS\\\""
starry_defs+=" -DAICP_INIT_FILE_TOKEN=\\\"AICP_STARRY_FILE\\\""
starry_defs+=" -DAICP_INIT_NETDIAG_TOKEN=\\\"AICP_STARRY_NETDIAG\\\""
starry_defs+=" -DAICP_INIT_STRESS_TOKEN=\\\"AICP_STARRY_STRESS\\\""
starry_defs+=" -DAICP_INIT_SERVER=\\\"${server}\\\""
starry_defs+=" -DAICP_INIT_SERVER_PORT=${server_port}u"
starry_defs+=" -DAICP_INIT_CLIENT=\\\"${client}\\\""
starry_defs+=" -DAICP_INIT_NET_PREFIX=\\\"${net_prefix}\\\""
starry_defs+=" -DAICP_INIT_NETMASK=\\\"${netmask}\\\""
starry_defs+=" -DAICP_INIT_STATIC_ARP=${static_arp}"
starry_defs+=" -DAICP_INIT_SERVER_MAC=\\\"${server_mac}\\\""
starry_defs+=" -DAICP_INIT_IFACE=\\\"${iface}\\\""
starry_defs+=" -DAICP_INIT_ITERATIONS=${iterations}u"
starry_defs+=" -DAICP_INIT_MODE=\\\"${mode}\\\""
starry_defs+=" -DAICP_INIT_CONNECT_RETRIES=${connect_retries}u"
starry_defs+=" -DAICP_INIT_TRANSPORT=\\\"${transport}\\\""
starry_defs+=" -DAICP_INIT_UDP_RETRIES=${udp_retries}u"
starry_defs+=" -DAICP_INIT_UDP_REORDER_TEST=${udp_reorder_test}"

make -C "${demo_dir}" -B starry-init-aarch64 AICP_STARRY_DEFS="${starry_defs}"
mkdir -p "${overlay_dir}/usr/bin"
install -m 0755 "${demo_dir}/build/aarch64/aicp_starry_init" "${overlay_dir}/usr/bin/aicp_starry_init"
install -m 0755 "${app_dir}/aicp-starry-run.sh" "${overlay_dir}/usr/bin/aicp-starry-run.sh"
install -m 0755 "${app_dir}/aicp-starry-run.sh" "${overlay_dir}/usr/bin/starry-run-case-tests"
