#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
# SPDX-License-Identifier: Apache-2.0

# Convert a source revision into a stable directory component so workflows
# that pin different upstream versions never share the same checkout.
aicp_revision_key() {
  local revision="$1"

  printf '%s' "${revision}" | LC_ALL=C tr -cs 'A-Za-z0-9._-' '-'
}

# Resolve a host command from an explicit environment override or PATH.
# Usage: aicp_resolve_tool DEBUGFS debugfs
aicp_resolve_tool() {
  local env_name="$1"
  local tool_name="$2"
  local configured="${!env_name:-}"

  if [[ -n "${configured}" ]]; then
    if command -v "${configured}" >/dev/null 2>&1; then
      command -v "${configured}"
      return 0
    fi
    if [[ -x "${configured}" ]]; then
      printf '%s\n' "${configured}"
      return 0
    fi
    echo "ERROR: ${env_name} 指向不可执行的命令：${configured}" >&2
    return 1
  fi

  if command -v "${tool_name}" >/dev/null 2>&1; then
    command -v "${tool_name}"
    return 0
  fi

  echo "ERROR: PATH 中未找到 ${tool_name}；请安装该工具或设置 ${env_name}=/path/to/${tool_name}" >&2
  return 1
}

aicp_resolve_cross_candidate() {
  local prefix="$1"
  local compiler

  compiler="$(command -v "${prefix}gcc" 2>/dev/null)" || return 1
  if [[ ! -x "${compiler}" ]]; then
    return 1
  fi

  case "${compiler}" in
    /*) ;;
    *) compiler="$(cd "$(dirname "${compiler}")" && pwd)/$(basename "${compiler}")" ;;
  esac
  printf '%s\n' "${compiler%gcc}"
}

aicp_cross_prefix_available() {
  aicp_resolve_cross_candidate "$1" >/dev/null
}

# Return success only when a cross compiler provides the hosted C headers
# required by third-party RTOS components such as FreeRTOS+TCP. A compiler-only
# Homebrew aarch64-elf installation is insufficient even though <prefix>gcc
# exists, because its sysroot does not contain newlib headers.
aicp_cross_prefix_has_c_library_headers() {
  local prefix="$1"
  local compiler="${prefix}gcc"

  printf '#include <string.h>\n' | "${compiler}" -E -x c - >/dev/null 2>&1
}

# Resolve a compiler prefix. The environment override is checked first, then
# each supplied prefix is tested by locating <prefix>gcc.
aicp_resolve_cross_prefix() {
  local env_name="$1"
  shift
  local configured="${!env_name:-}"
  local prefix resolved

  if [[ -n "${configured}" ]]; then
    if resolved="$(aicp_resolve_cross_candidate "${configured}")"; then
      printf '%s\n' "${resolved}"
      return 0
    fi
    echo "ERROR: ${env_name} 对应的编译器不存在：${configured}gcc" >&2
    return 1
  fi

  for prefix in "$@"; do
    if resolved="$(aicp_resolve_cross_candidate "${prefix}")"; then
      printf '%s\n' "${resolved}"
      return 0
    fi
  done

  echo "ERROR: 未找到 AArch64 交叉编译器；请设置 ${env_name}=/path/to/compiler-prefix-" >&2
  return 1
}

# Return success only when the reusable AArch64 Rust YOLOv8 runtime bundle is
# complete. Docker is a build-time dependency, not a runtime prerequisite.
aicp_yolo_rust_bundle_ready() {
  local install_dir="$1"
  local artifact
  local artifacts=(
    "aicp_yolov8_rust_onnx"
    "lib/ld-linux-aarch64.so.1"
    "lib/libonnxruntime.so.1.18.1"
    "model/yolov8n.onnx"
    "model/coco_80_labels_list.txt"
    "validation/images.txt"
    "validation/tennis-ball-close.jpg"
    "validation/tennis-ball-black-box.jpg"
    "validation/tennis-ball-plant.jpg"
  )

  [[ -x "${install_dir}/aicp_yolov8_rust_onnx" ]] || return 1
  for artifact in "${artifacts[@]}"; do
    [[ -f "${install_dir}/${artifact}" ]] || return 1
  done
}

aicp_arm_gnu_host_tag() {
  local os arch
  os="$(uname -s)"
  arch="$(uname -m)"

  case "${os}:${arch}" in
    Darwin:arm64|Darwin:aarch64) printf '%s\n' "darwin-arm64" ;;
    Linux:x86_64|Linux:amd64) printf '%s\n' "x86_64" ;;
    Linux:aarch64|Linux:arm64) printf '%s\n' "aarch64" ;;
    *)
      echo "ERROR: 不支持自动下载 Arm GNU Toolchain 的主机：${os}/${arch}" >&2
      return 1
      ;;
  esac
}

# Resolve a pinned Arm GNU AArch64 bare-metal toolchain. Existing explicit,
# repository-cached and PATH toolchains are reused before downloading.
aicp_resolve_or_install_aarch64_none_elf() {
  local repo_root="$1"
  local version="$2"
  local env_name="$3"
  local configured="${!env_name:-}"
  local prefix host_tag toolchain_name toolchain_dir archive url
  local curl_bin tar_bin
  local cached_compiler

  if [[ -n "${configured}" ]]; then
    prefix="$(aicp_resolve_cross_prefix "${env_name}" "${configured}")" || return 1
    if ! aicp_cross_prefix_has_c_library_headers "${prefix}"; then
      echo "ERROR: ${env_name} 对应的工具链缺少 string.h 等 C 运行库头文件：${prefix}" >&2
      return 1
    fi
    printf '%s\n' "${prefix}"
    return 0
  fi

  for cached_compiler in \
    "${repo_root}"/tmp/arm-gnu-toolchain-"${version}"-*-aarch64-none-elf/bin/aarch64-none-elf-gcc; do
    prefix="${cached_compiler%gcc}"
    if [[ -x "${cached_compiler}" ]] && aicp_cross_prefix_has_c_library_headers "${prefix}"; then
      printf '%s\n' "${prefix}"
      return 0
    fi
  done

  for prefix in aarch64-none-elf-; do
    if prefix="$(aicp_resolve_cross_candidate "${prefix}")"; then
      if aicp_cross_prefix_has_c_library_headers "${prefix}"; then
        printf '%s\n' "${prefix}"
        return 0
      fi
    fi
  done

  host_tag="$(aicp_arm_gnu_host_tag)" || {
    echo "请设置 ${env_name}=/path/to/aarch64-none-elf-" >&2
    return 1
  }
  toolchain_name="arm-gnu-toolchain-${version}-${host_tag}-aarch64-none-elf"
  toolchain_dir="${repo_root}/tmp/${toolchain_name}"
  archive="${repo_root}/tmp/${toolchain_name}.tar.xz"
  url="https://developer.arm.com/-/media/Files/downloads/gnu/${version}/binrel/${toolchain_name}.tar.xz"

  curl_bin="$(aicp_resolve_tool CURL curl)" || return 1
  tar_bin="$(aicp_resolve_tool TAR tar)" || return 1

  mkdir -p "${repo_root}/tmp"
  if [[ ! -f "${archive}" ]]; then
    echo "[ai-rtos] 下载 Arm GNU Toolchain ${version} (${host_tag})" >&2
    "${curl_bin}" -fL "${url}" -o "${archive}"
  fi
  if [[ ! -x "${toolchain_dir}/bin/aarch64-none-elf-gcc" ]]; then
    echo "[ai-rtos] 解压 Arm GNU Toolchain ${version}" >&2
    "${tar_bin}" -xJf "${archive}" -C "${repo_root}/tmp"
  fi

  prefix="${toolchain_dir}/bin/aarch64-none-elf-"
  if ! aicp_cross_prefix_has_c_library_headers "${prefix}"; then
    echo "ERROR: 下载的 Arm GNU Toolchain 缺少 C 运行库头文件：${prefix}" >&2
    return 1
  fi

  printf '%s\n' "${prefix}"
}
