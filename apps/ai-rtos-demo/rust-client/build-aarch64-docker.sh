#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
#
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
demo_dir="$(cd "${script_dir}/.." && pwd)"
out="${AICP_RUST_AARCH64_OUT:-${demo_dir}/build/aarch64/aicp_rust_client}"
image="${AICP_RUST_DOCKER_IMAGE:-narin-rootless-dev:latest}"
platform="${AICP_RUST_DOCKER_PLATFORM:-linux/arm64}"
target="${AICP_RUST_TARGET:-aarch64-unknown-linux-gnu}"

case "${target}" in
  aarch64-unknown-linux-gnu)
    linker="${AICP_RUST_LINKER:-aarch64-linux-gnu-gcc}"
    rustflags="${AICP_RUSTFLAGS:--C target-feature=+crt-static}"
    ;;
  aarch64-unknown-linux-musl)
    linker="${AICP_RUST_LINKER:-aarch64-linux-musl-gcc}"
    rustflags="${AICP_RUSTFLAGS:-}"
    ;;
  *)
    echo "ERROR: unsupported AICP_RUST_TARGET=${target}" >&2
    exit 2
    ;;
esac

mkdir -p "$(dirname "${out}")"

echo "[ai-rtos] Docker building Rust guest /init image=${image} platform=${platform} target=${target}"
docker run --rm \
  --platform "${platform}" \
  -v "${demo_dir}:/demo" \
  -w /demo/rust-client \
  -e "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=${linker}" \
  -e "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=${linker}" \
  -e "RUSTFLAGS=${rustflags}" \
  "${image}" \
  cargo build --release --target "${target}" --features guest-init

cp "${script_dir}/target/${target}/release/aicp_rust_client" "${out}"
chmod +x "${out}"
echo "[ai-rtos] Rust aarch64 guest binary: ${out}"
