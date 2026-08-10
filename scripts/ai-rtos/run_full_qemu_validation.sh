#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
#
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

usage() {
  cat <<'EOF'
用法：
  scripts/ai-rtos/run_full_qemu_validation.sh smoke
  scripts/ai-rtos/run_full_qemu_validation.sh full

验证档位：
  smoke  准备镜像并执行协议检查，以最小有效次数验证 Linux/StarryOS
         与 ArceOS/RT-Thread/Zephyr/FreeRTOS 组成的八组双 Guest 闭环。
  full   在八组闭环基础上增加控制效果对比、RT-Thread 可靠性、
         StarryOS UDP 故障恢复、Rust YOLOv8n、AxVisor 实时 A/B
         和三种原生 RTOS 周期基线。

可选环境变量：
  AICP_FULL_PREPARE_IMAGES=0|1          下载并准备 QEMU 镜像，默认 1
  AICP_FULL_ITERATIONS=N                双 Guest 闭环请求次数
  AICP_FULL_PROTOCOL_ITERATIONS=N       主机协议 smoke 请求次数
  AICP_FULL_BOOT_TIMEOUT=N              Guest 启动超时秒数
  AICP_FULL_STRESS_PROCS=N              Linux 压力进程数，默认 2
  AICP_FULL_INCLUDE_LONG_STABILITY=0|1  增加 1000/10000 次长稳
  AICP_FULL_DRY_RUN=0|1                 只打印阶段计划，不执行

结果目录：
  tmp/ai-rtos/results/full-validation-<timestamp>/summary.txt
  tmp/ai-rtos/results/full-validation-<timestamp>/logs/<stage>.log
EOF
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi
if [[ $# -ne 1 ]]; then
  usage >&2
  exit 2
fi

profile="$1"
case "${profile}" in
  smoke|full) ;;
  *)
    usage >&2
    exit 2
    ;;
esac

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
source "${repo_root}/scripts/ai-rtos/lib/host_tools.sh"
stamp="$(date +%Y%m%d-%H%M%S)"
result_dir="${repo_root}/tmp/ai-rtos/results/full-validation-${stamp}"
stage_log_dir="${result_dir}/logs"
summary_file="${result_dir}/summary.txt"
stage_file="${result_dir}/stages.tsv"

prepare_images="${AICP_FULL_PREPARE_IMAGES:-1}"
dry_run="${AICP_FULL_DRY_RUN:-0}"
stress_procs="${AICP_FULL_STRESS_PROCS:-2}"

if [[ "${profile}" == "smoke" ]]; then
  iterations="${AICP_FULL_ITERATIONS:-1}"
  protocol_iterations="${AICP_FULL_PROTOCOL_ITERATIONS:-5}"
  boot_timeout_s="${AICP_FULL_BOOT_TIMEOUT:-300}"
  include_long_stability="${AICP_FULL_INCLUDE_LONG_STABILITY:-0}"
else
  iterations="${AICP_FULL_ITERATIONS:-20}"
  protocol_iterations="${AICP_FULL_PROTOCOL_ITERATIONS:-50}"
  boot_timeout_s="${AICP_FULL_BOOT_TIMEOUT:-420}"
  include_long_stability="${AICP_FULL_INCLUDE_LONG_STABILITY:-0}"
fi

for flag_name in prepare_images dry_run include_long_stability; do
  flag_value="${!flag_name}"
  if [[ "${flag_value}" != "0" && "${flag_value}" != "1" ]]; then
    echo "ERROR: ${flag_name} must be 0 or 1, got '${flag_value}'" >&2
    exit 2
  fi
done
for integer_name in iterations protocol_iterations boot_timeout_s; do
  integer_value="${!integer_name}"
  if ! [[ "${integer_value}" =~ ^[1-9][0-9]*$ ]]; then
    echo "ERROR: ${integer_name} must be a positive integer, got '${integer_value}'" >&2
    exit 2
  fi
done
if ! [[ "${stress_procs}" =~ ^[0-9]+$ ]] || (( stress_procs > 16 )); then
  echo "ERROR: AICP_FULL_STRESS_PROCS must be in [0, 16], got '${stress_procs}'" >&2
  exit 2
fi

mkdir -p "${stage_log_dir}"
printf 'stage\tstatus\tduration_s\tlog\n' > "${stage_file}"
cat > "${summary_file}" <<EOF
profile=${profile}
started_at=${stamp}
prepare_images=${prepare_images}
iterations=${iterations}
protocol_iterations=${protocol_iterations}
boot_timeout_s=${boot_timeout_s}
stress_procs=${stress_procs}
include_long_stability=${include_long_stability}
dry_run=${dry_run}
result_dir=${result_dir}
EOF

format_command() {
  local formatted=""
  local argument
  for argument in "$@"; do
    printf -v formatted '%s %q' "${formatted}" "${argument}"
  done
  printf '%s' "${formatted# }"
}

run_stage() {
  local stage="$1"
  shift
  local log_file="${stage_log_dir}/${stage}.log"
  local command_text
  local started ended duration status

  command_text="$(format_command "$@")"
  echo "[ai-rtos] STAGE ${stage}: ${command_text}"
  if [[ "${dry_run}" == "1" ]]; then
    printf '%s\tDRY_RUN\t0\t%s\n' "${stage}" "${log_file}" >> "${stage_file}"
    return 0
  fi

  started="$(date +%s)"
  set +e
  "$@" 2>&1 | tee "${log_file}"
  status="${PIPESTATUS[0]}"
  set -e
  ended="$(date +%s)"
  duration=$((ended - started))

  if [[ "${status}" -ne 0 ]]; then
    printf '%s\tFAIL\t%s\t%s\n' "${stage}" "${duration}" "${log_file}" >> "${stage_file}"
    {
      echo "overall=FAIL"
      echo "failed_stage=${stage}"
      echo "failed_command=${command_text}"
      echo "failed_log=${log_file}"
    } >> "${summary_file}"
    cat "${stage_file}" >> "${summary_file}"
    cp "${summary_file}" "${repo_root}/tmp/ai-rtos/results/full-validation-latest.txt"
    echo "[ai-rtos] FAIL: stage=${stage}; see ${log_file}" >&2
    exit "${status}"
  fi

  printf '%s\tPASS\t%s\t%s\n' "${stage}" "${duration}" "${log_file}" >> "${stage_file}"
  echo "[ai-rtos] PASS: stage=${stage} duration_s=${duration}"
}

latest_file() {
  local pattern="$1"
  local file
  file="$(find "${repo_root}/tmp/ai-rtos/logs" -maxdepth 1 -type f -name "${pattern}" -print 2>/dev/null | sort | tail -n 1)"
  if [[ -z "${file}" ]]; then
    echo "ERROR: no log matches ${pattern}" >&2
    return 1
  fi
  printf '%s\n' "${file}"
}

check_runtime_isolation() {
  local name="$1"
  local ai_guest="$2"
  local network_profile="$3"
  local qemu_config="$4"
  local primary_pattern="$5"
  local secondary_pattern="${6:-}"
  local primary_log secondary_log
  local arguments

  primary_log="$(latest_file "${primary_pattern}")"
  arguments=(
    "${repo_root}/scripts/ai-rtos/check_aicp_network_isolation.py"
    --qemu-config "${qemu_config}"
    --profile "${network_profile}"
    --ai-guest "${ai_guest}"
    --log "${primary_log}"
    --summary "${result_dir}/isolation-${name}.txt"
  )
  if [[ -n "${secondary_pattern}" ]]; then
    secondary_log="$(latest_file "${secondary_pattern}")"
    arguments+=(--log "${secondary_log}")
  fi
  python3 "${arguments[@]}"
}

check_commands() {
  local command_name
  local missing=0
  local required=(cargo make python3 qemu-system-aarch64 dtc fdtoverlay mkfs.ext4 timeout cpio gzip perl lsof)
  local yolo_install_dir="${repo_root}/apps/ai-rtos-demo/yolov8-rust-onnx/install/aarch64"

  if [[ "${profile}" == "full" ]] &&
     { [[ "${AICP_YOLO_RUST_REBUILD:-0}" == "1" ]] ||
       ! aicp_yolo_rust_bundle_ready "${yolo_install_dir}"; }; then
    if [[ "${AICP_YOLO_RUST_SKIP_BUILD:-0}" == "1" ]]; then
      echo "missing_yolo_runtime_bundle=${yolo_install_dir}"
      missing=1
    else
      echo "yolo_runtime_bundle=build-required"
      required+=(docker)
    fi
  elif [[ "${profile}" == "full" ]]; then
    echo "yolo_runtime_bundle=reused path=${yolo_install_dir}"
  fi
  for command_name in "${required[@]}"; do
    if ! command -v "${command_name}" >/dev/null 2>&1; then
      echo "missing_command=${command_name}"
      missing=1
    else
      echo "command=${command_name} path=$(command -v "${command_name}")"
    fi
  done
  if (( missing != 0 )); then
    return 1
  fi
}

cd "${repo_root}"

run_stage preflight check_commands
run_stage shell_syntax scripts/ai-rtos/check_shell_syntax.sh
run_stage host_tools_unit scripts/ai-rtos/test_host_tools.sh
run_stage run_artifacts_unit scripts/ai-rtos/test_run_artifacts.sh
run_stage python_syntax python3 -m py_compile scripts/ai-rtos/*.py
run_stage isolation_unit python3 -m unittest scripts/ai-rtos/test_check_aicp_network_isolation.py
run_stage architecture scripts/ai-rtos/check_demo_architecture.sh
run_stage third_party_clean scripts/ai-rtos/check_third_party_sources_clean.sh
run_stage host_build make -C apps/ai-rtos-demo all
run_stage protocol_unit make -C apps/ai-rtos-demo test

if [[ "${prepare_images}" == "1" ]]; then
  run_stage prepare_images scripts/ai-rtos/setup_qemu_rtos.sh all
fi

run_stage protocol_reliability scripts/ai-rtos/run_aicp_protocol_reliability.sh "${protocol_iterations}"

run_stage linux_arceos scripts/ai-rtos/run_axvisor_dual_guest_aicp.sh "${iterations}" ai "${boot_timeout_s}"
run_stage isolation_linux_arceos check_runtime_isolation \
  linux-arceos linux arceos-tcp \
  "${repo_root}/tmp/ai-rtos/axvisor-dual-guest-aicp-c-qemu.generated.toml" \
  'axvisor-dual-guest-aicp-c-[0-9]*.log' \
  'axvisor-dual-guest-aicp-c-linux-console-*.log'

run_stage linux_rtthread scripts/ai-rtos/run_axvisor_linux_rtthread_aicp.sh "${iterations}" ai "${boot_timeout_s}"
run_stage isolation_linux_rtthread check_runtime_isolation \
  linux-rtthread linux rtthread-tcp \
  "${repo_root}/tmp/ai-rtos/qemu-aarch64-linux-rtthread.toml" \
  'axvisor-linux-rtthread-control-aicp-*.log' 'linux-rtthread-console-*.log'

run_stage linux_zephyr scripts/ai-rtos/run_axvisor_linux_zephyr_aicp.sh "${iterations}" ai "${boot_timeout_s}"
run_stage isolation_linux_zephyr check_runtime_isolation \
  linux-zephyr linux zephyr-tcp \
  "${repo_root}/tmp/ai-rtos/qemu-aarch64-linux-zephyr.toml" \
  'axvisor-linux-zephyr-aicp-*.log' 'linux-zephyr-console-*.log'

run_stage linux_freertos scripts/ai-rtos/run_axvisor_linux_freertos_aicp.sh "${iterations}" ai "${boot_timeout_s}"
run_stage isolation_linux_freertos check_runtime_isolation \
  linux-freertos linux freertos-tcp \
  "${repo_root}/tmp/ai-rtos/qemu-aarch64-linux-freertos.toml" \
  'axvisor-linux-freertos-aicp-*.log' 'linux-freertos-console-*.log'

for rtos_guest in arceos rtthread zephyr freertos; do
  run_stage "starry_${rtos_guest}" env \
    AICP_STARRY_NATIVE=1 \
    AICP_QEMU_NET_BACKEND=hub \
    AICP_STARRY_TRANSPORT=tcp \
    AICP_RTOS_GUEST="${rtos_guest}" \
    scripts/ai-rtos/run_axvisor_starry_rtos_aicp.sh "${iterations}" ai "${boot_timeout_s}"
  run_stage "isolation_starry_${rtos_guest}" check_runtime_isolation \
    "starry-${rtos_guest}" starry "${rtos_guest}-tcp" \
    "${repo_root}/tmp/ai-rtos/qemu-aarch64-aicp-starry-dual.generated.toml" \
    "axvisor-starry-${rtos_guest}-aicp-*.log"
done

if [[ "${profile}" == "full" ]]; then
  run_stage control_compare env AICP_ITERATIONS=100 scripts/ai-rtos/run_aicp_control_compare.sh
  run_stage rtthread_reliability scripts/ai-rtos/run_axvisor_rtthread_reliability.sh 20 "${boot_timeout_s}"
  run_stage starry_udp_faults scripts/ai-rtos/run_axvisor_starry_udp_faults.sh 20
  run_stage rust_yolov8_rtthread scripts/ai-rtos/run_axvisor_rtthread_yolov8_rust_aicp.sh "${boot_timeout_s}"
  run_stage realtime_before_after scripts/ai-rtos/run_axvisor_rt_before_after.sh 300 "${boot_timeout_s}" "${stress_procs}"
  run_stage baseline_zephyr scripts/ai-rtos/run_zephyr_periodic_baseline.sh
  run_stage baseline_rtthread scripts/ai-rtos/run_rtthread_periodic_baseline.sh
  run_stage baseline_freertos scripts/ai-rtos/run_freertos_periodic_baseline.sh

  if [[ "${include_long_stability}" == "1" ]]; then
    run_stage long_rtthread scripts/ai-rtos/run_axvisor_rtthread_long_stability.sh 1000 900 "${stress_procs}"
    run_stage long_arceos scripts/ai-rtos/run_axvisor_long_stability.sh 10000 4200 "${stress_procs}"
  fi
fi

if [[ "${dry_run}" == "1" ]]; then
  echo "overall=DRY_RUN" >> "${summary_file}"
else
  echo "overall=PASS" >> "${summary_file}"
fi
echo >> "${summary_file}"
cat "${stage_file}" >> "${summary_file}"
cp "${summary_file}" "${repo_root}/tmp/ai-rtos/results/full-validation-latest.txt"

cat "${summary_file}"
echo "[ai-rtos] ${profile} validation complete: ${result_dir}"
