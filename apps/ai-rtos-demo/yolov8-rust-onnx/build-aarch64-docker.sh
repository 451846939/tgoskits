#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
#
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="${AICP_TGOSKITS_ROOT:-$(cd "${script_dir}/../../.." && pwd)}"
image="${AICP_RUST_DOCKER_IMAGE:-narin-rootless-dev:latest}"
cpp_demo="${repo_root}/apps/ai-rtos-demo/yolov8-onnx-cpu"

if ! docker info >/dev/null 2>&1; then
  echo "ERROR: Docker daemon is not running or the current user cannot access its socket" >&2
  exit 1
fi

if ! docker image inspect "${image}" >/dev/null 2>&1; then
  echo "ERROR: Docker image ${image} is unavailable" >&2
  echo "The image must provide Rust, aarch64-unknown-linux-gnu, and aarch64-linux-gnu-gcc." >&2
  exit 1
fi

for path in \
  "${cpp_demo}/third_party/onnxruntime/include/onnxruntime_c_api.h" \
  "${cpp_demo}/third_party/onnxruntime/lib/libonnxruntime.so" \
  "${cpp_demo}/model/yolov8n.onnx" \
  "${cpp_demo}/validation/images.txt"; do
  if [[ ! -e "${path}" ]]; then
    echo "ERROR: missing shared YOLOv8 CPU asset: ${path}" >&2
    echo "Run apps/ai-rtos-demo/yolov8-onnx-cpu/build-docker.sh first." >&2
    exit 1
  fi
done

docker run --rm \
  -v "${repo_root}:/tgoskits:ro" \
  -v "${script_dir}:/demo" \
  -w /demo \
  "${image}" \
  bash -lc '
set -euo pipefail

target=aarch64-unknown-linux-gnu
ort=/tgoskits/apps/ai-rtos-demo/yolov8-onnx-cpu/third_party/onnxruntime
cpp_install=/tgoskits/apps/ai-rtos-demo/yolov8-onnx-cpu/install/aarch64
stb=/tgoskits/apps/starry/orangepi-5-plus-uvc-rknn/rknn-yolov8-image/3rdparty/stb_image
build=/demo/build/aarch64
install=/demo/install/aarch64

command -v cargo >/dev/null
command -v aarch64-linux-gnu-gcc >/dev/null
command -v aarch64-linux-gnu-ar >/dev/null
rustup target list --installed | grep -qx "${target}"

rm -rf "${build}" "${install}"
mkdir -p "${build}/ffi" "${install}/lib" "${install}/model" "${install}/validation"

aarch64-linux-gnu-gcc \
  -O3 -DNDEBUG -fPIC -std=c11 -Wall -Wextra -Werror \
  -Wno-sign-compare -Wno-unused-but-set-variable \
  -I/demo/ffi -I"${ort}/include" -I"${stb}" \
  -c /demo/ffi/ort_shim.c \
  -o "${build}/ffi/ort_shim.o"
aarch64-linux-gnu-ar rcs "${build}/ffi/libaicp_yolo_ffi.a" "${build}/ffi/ort_shim.o"

export CARGO_TARGET_DIR="${build}/cargo"
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
export RUSTFLAGS="-L native=${build}/ffi -l static=aicp_yolo_ffi -L native=${ort}/lib -l dylib=onnxruntime -l dylib=m -C link-arg=-Wl,-rpath,/lib"
cargo build --manifest-path /demo/Cargo.toml --release --target "${target}"

cp "${build}/cargo/${target}/release/aicp-yolov8-rust-onnx" "${install}/aicp_yolov8_rust_onnx"
cp -a "${ort}/lib"/libonnxruntime.so* "${install}/lib/"

if [[ -d "${cpp_install}/lib" ]]; then
  cp -a "${cpp_install}/lib"/. "${install}/lib/"
else
  copy_library() {
    local library="$1"
    if [[ -f "${library}" ]]; then
      cp -aL "${library}" "${install}/lib/"
    fi
  }
  for library in \
    /usr/lib/aarch64-linux-gnu/ld-linux-aarch64.so.1 \
    /usr/lib/aarch64-linux-gnu/libc.so.6 \
    /usr/lib/aarch64-linux-gnu/libm.so.6 \
    /usr/lib/aarch64-linux-gnu/libpthread.so.0 \
    /usr/lib/aarch64-linux-gnu/libdl.so.2 \
    /usr/lib/aarch64-linux-gnu/librt.so.1 \
    /usr/aarch64-linux-gnu/lib/ld-linux-aarch64.so.1 \
    /usr/aarch64-linux-gnu/lib/libc.so.6 \
    /usr/aarch64-linux-gnu/lib/libm.so.6 \
    /usr/aarch64-linux-gnu/lib/libpthread.so.0 \
    /usr/aarch64-linux-gnu/lib/libdl.so.2 \
    /usr/aarch64-linux-gnu/lib/librt.so.1 \
    "$(aarch64-linux-gnu-gcc -print-file-name=libgcc_s.so.1)"; do
    copy_library "${library}"
  done
fi

cp /tgoskits/apps/ai-rtos-demo/yolov8-onnx-cpu/model/yolov8n.onnx "${install}/model/"
cp /tgoskits/apps/ai-rtos-demo/yolov8-onnx-cpu/model/coco_80_labels_list.txt "${install}/model/"
cp /tgoskits/apps/ai-rtos-demo/yolov8-onnx-cpu/validation/images.txt "${install}/validation/"
cp /tgoskits/apps/ai-rtos-demo/yolov8-onnx-cpu/validation/*.jpg "${install}/validation/"

if command -v file >/dev/null 2>&1; then
  file "${install}/aicp_yolov8_rust_onnx"
else
  aarch64-linux-gnu-readelf -h "${install}/aicp_yolov8_rust_onnx" \
    | grep -E "Class:|Machine:"
fi
aarch64-linux-gnu-readelf -d "${install}/aicp_yolov8_rust_onnx" | grep -E "NEEDED|RUNPATH"
test -f "${install}/lib/ld-linux-aarch64.so.1"
test -f "${install}/lib/libc.so.6"
test -f "${install}/lib/libonnxruntime.so"
echo "[aicp-yolo-rust] install: ${install}"
'
