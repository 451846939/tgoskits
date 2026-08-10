#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
#
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
demo_dir="$(cd "${script_dir}/.." && pwd)"
out="${AICP_RUST_AARCH64_OUT:-${demo_dir}/build/aarch64/aicp_rust_client}"
build_mode="${AICP_RUST_BUILD_MODE:-auto}"
local_target="${AICP_RUST_LOCAL_TARGET:-aarch64-unknown-linux-musl}"
local_linker="${AICP_RUST_LOCAL_LINKER:-aarch64-linux-musl-gcc}"

usage() {
  cat <<'EOF'
用法：
  rust-client/build-aarch64.sh

环境变量：
  AICP_RUST_BUILD_MODE     auto、local 或 docker，默认 auto
  AICP_RUST_AARCH64_OUT    输出文件路径
  AICP_RUST_LOCAL_TARGET   本地构建目标，默认 aarch64-unknown-linux-musl
  AICP_RUST_LOCAL_LINKER   本地交叉链接器，默认 aarch64-linux-musl-gcc

auto 模式在本地 Rust target 和交叉链接器均可用时执行本地静态构建，
否则调用 build-aarch64-docker.sh。
EOF
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi
if [[ $# -ne 0 ]]; then
  usage >&2
  exit 2
fi
if [[ "${build_mode}" != "auto" && "${build_mode}" != "local" && "${build_mode}" != "docker" ]]; then
  echo "ERROR: AICP_RUST_BUILD_MODE 必须为 auto、local 或 docker，实际为 '${build_mode}'" >&2
  exit 2
fi

local_build_available() {
  command -v cargo >/dev/null 2>&1 &&
    command -v "${local_linker}" >/dev/null 2>&1 &&
    rustup target list --installed 2>/dev/null | grep -Fxq "${local_target}"
}

build_local() {
  local linker_env

  case "${local_target}" in
    aarch64-unknown-linux-musl)
      linker_env="CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER"
      ;;
    aarch64-unknown-linux-gnu)
      linker_env="CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER"
      ;;
    *)
      echo "ERROR: 本地构建暂不支持目标 ${local_target}" >&2
      exit 2
      ;;
  esac

  echo "[ai-rtos] 本地交叉构建 Rust Guest：target=${local_target} linker=${local_linker}"
  (
    cd "${script_dir}"
    env "${linker_env}=${local_linker}" \
      cargo build --release --target "${local_target}" --features guest-init
  )
  install -m 0755 \
    "${script_dir}/target/${local_target}/release/aicp_rust_client" \
    "${out}"
}

mkdir -p "$(dirname "${out}")"

case "${build_mode}" in
  local)
    if ! local_build_available; then
      echo "ERROR: 本地 Rust target 或交叉链接器不可用：target=${local_target} linker=${local_linker}" >&2
      exit 1
    fi
    build_local
    ;;
  docker)
    AICP_RUST_AARCH64_OUT="${out}" "${script_dir}/build-aarch64-docker.sh"
    ;;
  auto)
    if local_build_available; then
      build_local
    elif command -v docker >/dev/null 2>&1; then
      echo "[ai-rtos] 本地交叉工具链不可用，回退到 Docker 构建"
      AICP_RUST_AARCH64_OUT="${out}" "${script_dir}/build-aarch64-docker.sh"
    else
      echo "ERROR: 既没有可用的本地交叉工具链，也没有 docker" >&2
      echo "请安装 ${local_target} 和 ${local_linker}，或安装 Docker。" >&2
      exit 1
    fi
    ;;
esac

echo "[ai-rtos] Rust AArch64 Guest 二进制：${out}"
