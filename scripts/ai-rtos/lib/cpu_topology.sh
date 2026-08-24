#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
#
# SPDX-License-Identifier: Apache-2.0

# Resolve and validate the four-core-oriented dual-guest topology used by the
# AICP QEMU demos. The AI guest owns two consecutive pCPUs because the current
# GPPT GICR device describes one contiguous redistributor-frame range.
aicp_configure_dual_guest_cpu_topology() {
  host_cpus="${AICP_HOST_CPUS:-4}"
  housekeeping_pcpu="${AICP_HOUSEKEEPING_PCPU:-0}"
  rtos_vcpu0_pcpu="${AICP_RTOS_VCPU0_PCPU:-1}"
  linux_vcpu0_pcpu="${AICP_LINUX_VCPU0_PCPU:-2}"
  linux_vcpu1_pcpu="${AICP_LINUX_VCPU1_PCPU:-3}"

  local name value
  for name in host_cpus housekeeping_pcpu rtos_vcpu0_pcpu linux_vcpu0_pcpu linux_vcpu1_pcpu; do
    value="${!name}"
    if ! [[ "${value}" =~ ^[0-9]+$ ]]; then
      echo "ERROR: CPU topology values must be non-negative integers; ${name}='${value}'" >&2
      return 2
    fi
  done

  if ((host_cpus < 4 || host_cpus > 63)); then
    echo "ERROR: AICP_HOST_CPUS must be in [4, 63], got '${host_cpus}'" >&2
    return 2
  fi

  local pcpu
  for pcpu in "${housekeeping_pcpu}" "${rtos_vcpu0_pcpu}"     "${linux_vcpu0_pcpu}" "${linux_vcpu1_pcpu}"; do
    if ((pcpu >= host_cpus)); then
      echo "ERROR: pCPU ${pcpu} is outside AICP_HOST_CPUS=${host_cpus}" >&2
      return 2
    fi
  done

  if ((housekeeping_pcpu == rtos_vcpu0_pcpu ||
       housekeeping_pcpu == linux_vcpu0_pcpu ||
       housekeeping_pcpu == linux_vcpu1_pcpu ||
       rtos_vcpu0_pcpu == linux_vcpu0_pcpu ||
       rtos_vcpu0_pcpu == linux_vcpu1_pcpu ||
       linux_vcpu0_pcpu == linux_vcpu1_pcpu)); then
    echo "ERROR: housekeeping, RTOS, and AI guest vCPUs must use distinct host pCPUs" >&2
    return 2
  fi

  if ((linux_vcpu1_pcpu != linux_vcpu0_pcpu + 1)); then
    echo "ERROR: the two AI guest pCPUs must be consecutive for the GPPT GICR range" >&2
    return 2
  fi

  linux_vcpu0_mask=$((1 << linux_vcpu0_pcpu))
  linux_vcpu1_mask=$((1 << linux_vcpu1_pcpu))
  rtos_vcpu0_mask=$((1 << rtos_vcpu0_pcpu))
}
