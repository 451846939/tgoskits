#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
script_dir="${repo_root}/scripts/ai-rtos"
source "${script_dir}/lib/host_tools.sh"

usage() {
  cat <<'EOF'
用法：
  scripts/ai-rtos/aicp.sh doctor
  scripts/ai-rtos/aicp.sh prepare
  scripts/ai-rtos/aicp.sh smoke
  scripts/ai-rtos/aicp.sh full
  scripts/ai-rtos/aicp.sh run <linux|starry> <arceos|rtthread|zephyr|freertos> [次数] [ai|fixed] [超时秒数]
  scripts/ai-rtos/aicp.sh yolov8 [超时秒数]
  scripts/ai-rtos/aicp.sh realtime [次数] [超时秒数] [Linux 压力进程数]
  scripts/ai-rtos/aicp.sh baseline <rtthread|zephyr|freertos|all>
  scripts/ai-rtos/aicp.sh reliability [次数] [超时秒数]
  scripts/ai-rtos/aicp.sh list

常用命令：
  doctor      只检查宿主工具和环境，不下载、不构建
  prepare     拉取 QEMU 镜像并生成基础 Guest 配置
  smoke       最小次数验证八组 Linux/StarryOS + 控制 Guest 闭环
  full        执行完整闭环、可靠性、YOLOv8、实时 A/B 和 RTOS 基线
  run         单独运行一组双 Guest，便于演示和排障
  yolov8      运行 Linux Rust YOLOv8n CPU + RT-Thread 控制闭环
  realtime    执行 AxVisor 实时优化前后 A/B 对比
  baseline    执行原生 RTOS 20 ms 周期任务空载/压力基线
  reliability 执行 RT-Thread 协议可靠性与异常恢复测试
  list        显示入口和底层脚本的职责分类

工具路径：
  DEBUGFS、E2FSCK、RESIZE2FS 等环境变量优先；未设置时从 PATH 查找。
  交叉编译器可通过 RTTHREAD_CC_PREFIX、CROSS_COMPILE、
  ZEPHYR_CROSS_COMPILE 等变量传入。变量值可以是绝对前缀，也可以是
  PATH 中可解析的命令前缀。
EOF
}

require_arg_count() {
  local minimum="$1"
  local maximum="$2"
  local actual="$3"
  if (( actual < minimum || actual > maximum )); then
    usage >&2
    exit 2
  fi
}

doctor() {
  local missing=0
  local tool resolved
  local required=(
    cargo make python3 qemu-system-aarch64 dtc fdtoverlay fdtget
    mkfs.ext4 timeout cpio gzip perl lsof cmake ninja git curl tar
  )

  echo "[ai-rtos] 宿主工具检查"
  for tool in "${required[@]}"; do
    if resolved="$(command -v "${tool}" 2>/dev/null)"; then
      printf '  %-24s %s\n' "${tool}" "${resolved}"
    else
      printf '  %-24s %s\n' "${tool}" "缺失"
      missing=1
    fi
  done

  echo
  echo "[ai-rtos] ext4 工具解析"
  for tool in DEBUGFS:debugfs E2FSCK:e2fsck RESIZE2FS:resize2fs; do
    local env_name="${tool%%:*}"
    local command_name="${tool#*:}"
    if resolved="$(aicp_resolve_tool "${env_name}" "${command_name}" 2>/dev/null)"; then
      printf '  %-24s %s\n' "${env_name}" "${resolved}"
    else
      printf '  %-24s %s\n' "${env_name}" "缺失（可设置 ${env_name}）"
      missing=1
    fi
  done

  echo
  echo "[ai-rtos] 交叉编译器检查（bare-metal 工具链可自动下载，Zephyr musl 工具链需安装或配置）"
  if resolved="$(aicp_resolve_cross_prefix AICP_DOCTOR_MUSL_PREFIX aarch64-linux-musl- 2>/dev/null)"; then
    printf '  %-24s %s\n' "aarch64-linux-musl" "${resolved}gcc"
  else
    printf '  %-24s %s\n' "aarch64-linux-musl" "未发现"
  fi
  if resolved="$(aicp_resolve_cross_prefix AICP_DOCTOR_BARE_PREFIX aarch64-none-elf- aarch64-elf- 2>/dev/null)"; then
    printf '  %-24s %s\n' "aarch64-none-elf" "${resolved}gcc"
  else
    printf '  %-24s %s\n' "aarch64-none-elf" "未发现"
  fi

  if command -v docker >/dev/null 2>&1; then
    printf '  %-24s %s\n' "docker（YOLOv8 构建）" "$(command -v docker)"
  else
    printf '  %-24s %s\n' "docker（YOLOv8 构建）" "未发现；仅影响容器交叉构建"
  fi

  if (( missing != 0 )); then
    echo >&2
    echo "[ai-rtos] 工具检查未通过。macOS 可用 brew --prefix 动态加入 keg-only 工具目录，或显式设置对应环境变量。" >&2
    return 1
  fi
  echo
  echo "[ai-rtos] PASS：必需宿主工具均可解析"
}

run_pair() {
  local ai_guest="$1"
  local rtos_guest="$2"
  shift 2

  case "${ai_guest}:${rtos_guest}" in
    linux:arceos)
      exec "${script_dir}/run_axvisor_dual_guest_aicp.sh" "$@"
      ;;
    linux:rtthread)
      exec "${script_dir}/run_axvisor_linux_rtthread_aicp.sh" "$@"
      ;;
    linux:zephyr)
      exec "${script_dir}/run_axvisor_linux_zephyr_aicp.sh" "$@"
      ;;
    linux:freertos)
      exec "${script_dir}/run_axvisor_linux_freertos_aicp.sh" "$@"
      ;;
    starry:arceos|starry:rtthread|starry:zephyr|starry:freertos)
      AICP_STARRY_NATIVE=1 \
        AICP_QEMU_NET_BACKEND=hub \
        AICP_STARRY_TRANSPORT=tcp \
        AICP_RTOS_GUEST="${rtos_guest}" \
        exec "${script_dir}/run_axvisor_starry_rtos_aicp.sh" "$@"
      ;;
    *)
      echo "ERROR：不支持的 Guest 组合：${ai_guest} + ${rtos_guest}" >&2
      usage >&2
      exit 2
      ;;
  esac
}

list_scripts() {
  cat <<'EOF'
推荐入口：
  aicp.sh                         环境检查、准备、矩阵、单组合和专项测试入口

内部编排器：
  run_full_qemu_validation.sh     smoke/full 全矩阵编排和阶段日志汇总
  setup_qemu_rtos.sh              基础镜像拉取与 ArceOS/Zephyr 配置生成

Guest 构建：
  build_rtthread_aicp_guest.sh    构建 RT-Thread AICP Guest
  build_zephyr_aicp_guest.sh      构建 Zephyr AICP Guest
  build_freertos_aicp_guest.sh    构建 FreeRTOS AICP Guest

双 Guest 主线：
  run_axvisor_dual_guest_aicp.sh          Linux + ArceOS
  run_axvisor_linux_rtthread_aicp.sh      Linux + RT-Thread
  run_axvisor_linux_zephyr_aicp.sh        Linux + Zephyr
  run_axvisor_linux_freertos_aicp.sh      Linux + FreeRTOS
  run_axvisor_starry_rtos_aicp.sh         StarryOS + 四种控制 Guest

专项验证：
  run_axvisor_rtthread_yolov8_rust_aicp.sh  Rust YOLOv8n CPU 控制闭环
  run_axvisor_rt_before_after.sh             AxVisor 实时优化 A/B
  run_axvisor_rtthread_reliability.sh        RT-Thread 可靠性与恢复
  run_*_periodic_baseline.sh                 三种原生 RTOS 周期基线
  run_*_long_stability.sh                    长时间稳定性

辅助检查与数据处理：
  check_*.sh、extract_*.py、summarize_*.py、compare_control.py
  由上层脚本调用，普通复现通常不需要直接执行。

完整命令、环境变量、脚本字典和失败重跑方法见：
  docs/ai-rtos/完整全流程实现与复现手册.md
EOF
}

command_name="${1:---help}"
case "${command_name}" in
  -h|--help|help)
    usage
    ;;
  doctor)
    require_arg_count 1 1 "$#"
    doctor
    ;;
  prepare)
    require_arg_count 1 1 "$#"
    exec "${script_dir}/setup_qemu_rtos.sh" all
    ;;
  smoke|full)
    require_arg_count 1 1 "$#"
    exec "${script_dir}/run_full_qemu_validation.sh" "${command_name}"
    ;;
  run)
    require_arg_count 3 6 "$#"
    run_pair "$2" "$3" "${@:4}"
    ;;
  yolov8)
    require_arg_count 1 2 "$#"
    exec "${script_dir}/run_axvisor_rtthread_yolov8_rust_aicp.sh" "${2:-420}"
    ;;
  realtime)
    require_arg_count 1 4 "$#"
    exec "${script_dir}/run_axvisor_rt_before_after.sh" "${@:2}"
    ;;
  baseline)
    require_arg_count 2 2 "$#"
    case "$2" in
      rtthread)
        exec "${script_dir}/run_rtthread_periodic_baseline.sh"
        ;;
      zephyr)
        exec "${script_dir}/run_zephyr_periodic_baseline.sh"
        ;;
      freertos)
        exec "${script_dir}/run_freertos_periodic_baseline.sh"
        ;;
      all)
        "${script_dir}/run_rtthread_periodic_baseline.sh"
        "${script_dir}/run_zephyr_periodic_baseline.sh"
        "${script_dir}/run_freertos_periodic_baseline.sh"
        ;;
      *)
        usage >&2
        exit 2
        ;;
    esac
    ;;
  reliability)
    require_arg_count 1 3 "$#"
    exec "${script_dir}/run_axvisor_rtthread_reliability.sh" "${@:2}"
    ;;
  list)
    require_arg_count 1 1 "$#"
    list_scripts
    ;;
  *)
    echo "ERROR：未知命令：${command_name}" >&2
    usage >&2
    exit 2
    ;;
esac
