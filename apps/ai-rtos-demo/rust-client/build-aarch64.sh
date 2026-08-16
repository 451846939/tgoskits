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

auto 模式在本地 Rust target 已安装，或固定 nightly 提供 rust-src 时执行
本地静态构建；否则调用 build-aarch64-docker.sh。
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
  local rust_library

  command -v cargo >/dev/null 2>&1 || return 1
  command -v "${local_linker}" >/dev/null 2>&1 || return 1
  if rustup target list --installed 2>/dev/null | grep -Fxq "${local_target}"; then
    return 0
  fi
  rust_library="$(rustc --print sysroot)/lib/rustlib/src/rust/library"
  [[ -d "${rust_library}/std" ]]
}

build_local() {
  local linker_env
  local -a cargo_args

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
  cargo_args=(build --release --target "${local_target}" --features guest-init)
  if ! rustup target list --installed 2>/dev/null | grep -Fxq "${local_target}"; then
    local unwind_archive link_support_dir rustflags
    unwind_archive="$("${local_linker}" -print-file-name=libgcc_eh.a)"
    if [[ ! -f "${unwind_archive}" ]]; then
      echo "ERROR: build-std 需要链接器提供 libgcc_eh.a：${unwind_archive}" >&2
      exit 1
    fi
    link_support_dir="${script_dir}/target/aicp-rust-link/${local_target}"
    mkdir -p "${link_support_dir}"
    ln -sf "${unwind_archive}" "${link_support_dir}/libunwind.a"
    rustflags="${RUSTFLAGS:-} -C link-self-contained=no -L native=${link_support_dir}"
    cargo_args+=("-Z" "build-std=std,panic_abort")
    echo "[ai-rtos] Rust target 未预装，使用 rust-src 和 -Z build-std"
  else
    rustflags="${RUSTFLAGS:-}"
  fi
  (
    cd "${script_dir}"
    env "${linker_env}=${local_linker}" RUSTFLAGS="${rustflags# }" \
      cargo "${cargo_args[@]}"
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
      echo "请安装 ${local_linker} 以及 ${local_target} 或 rust-src，或安装 Docker。" >&2
      exit 1
    fi
    ;;
esac

echo "[ai-rtos] Rust AArch64 Guest 二进制：${out}"
