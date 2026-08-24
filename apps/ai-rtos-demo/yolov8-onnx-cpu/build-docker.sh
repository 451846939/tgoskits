#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
#
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
image="${AICP_DOCKER_IMAGE:-clion-ubuntu:24.04}"

if ! docker image inspect "${image}" >/dev/null 2>&1; then
  image="ubuntu:24.04"
fi

docker run --rm \
  -v "${repo_root}:/work" \
  -w /work \
  "${image}" \
  bash -lc '
set -euo pipefail

export DEBIAN_FRONTEND=noninteractive
need_apt=0
for tool in curl cmake make aarch64-linux-gnu-g++ file python3; do
  if ! command -v "${tool}" >/dev/null 2>&1; then
    need_apt=1
  fi
done
if [[ "${need_apt}" == "1" ]]; then
  apt-get update
  apt-get install -y --no-install-recommends \
    ca-certificates curl cmake make file \
    gcc-aarch64-linux-gnu g++-aarch64-linux-gnu \
    python3 python3-pip
fi

demo_dir=apps/ai-rtos-demo/yolov8-onnx-cpu
third_party="${demo_dir}/third_party"
download_dir="${third_party}/downloads"
model_dir="${demo_dir}/model"
validation_src=apps/starry/orangepi-5-plus-uvc-rknn/rknn-yolov8-image/validation
label_src=apps/starry/orangepi-5-plus-uvc-rknn/rknn-yolov8-image/model/coco_80_labels_list.txt
ort_version="${ONNXRUNTIME_VERSION:-1.18.1}"
ort_pkg="onnxruntime-linux-aarch64-${ort_version}"
ort_tgz="${download_dir}/${ort_pkg}.tgz"
ort_url="https://github.com/microsoft/onnxruntime/releases/download/v${ort_version}/${ort_pkg}.tgz"
install_dir="${demo_dir}/install/aarch64"
build_dir="${demo_dir}/build/aarch64"

mkdir -p "${download_dir}" "${model_dir}" "${install_dir}/lib"
rm -rf "${install_dir}/lib"
mkdir -p "${install_dir}/lib"

if [[ ! -f "${ort_tgz}" ]]; then
  echo "[aicp-yolo-cpu] downloading ${ort_url}"
  curl -L --retry 5 --retry-delay 2 -o "${ort_tgz}" "${ort_url}"
fi
rm -rf "${third_party}/${ort_pkg}" "${third_party}/onnxruntime"
tar -xzf "${ort_tgz}" -C "${third_party}"
mv "${third_party}/${ort_pkg}" "${third_party}/onnxruntime"

cp "${label_src}" "${model_dir}/coco_80_labels_list.txt"
rm -rf "${demo_dir}/validation"
mkdir -p "${demo_dir}/validation"
cp "${validation_src}"/*.jpg "${validation_src}/images.txt" "${demo_dir}/validation/"

if [[ ! -f "${model_dir}/yolov8n.onnx" ]]; then
  urls=()
  if [[ -n "${YOLOV8_ONNX_URL:-}" ]]; then
    urls+=("${YOLOV8_ONNX_URL}")
  fi
  urls+=(
    "https://github.com/ultralytics/assets/releases/download/v8.3.0/yolov8n.onnx"
    "https://github.com/ultralytics/assets/releases/download/v8.2.0/yolov8n.onnx"
    "https://github.com/ultralytics/assets/releases/download/v8.1.0/yolov8n.onnx"
  )
  for url in "${urls[@]}"; do
    echo "[aicp-yolo-cpu] trying model URL ${url}"
    if curl -L --fail --retry 3 --retry-delay 2 -o "${model_dir}/yolov8n.onnx.tmp" "${url}"; then
      mv "${model_dir}/yolov8n.onnx.tmp" "${model_dir}/yolov8n.onnx"
      break
    fi
  done
  rm -f "${model_dir}/yolov8n.onnx.tmp"
fi

if [[ ! -f "${model_dir}/yolov8n.onnx" ]]; then
  echo "[aicp-yolo-cpu] direct ONNX asset unavailable; exporting with ultralytics"
  # Export happens on the host side only.  Pull the CPU wheel explicitly so a
  # GPU-enabled PyTorch resolver cannot download CUDA packages that are never
  # used by this ONNX Runtime CPU deployment.
  python3 -m pip install --break-system-packages --no-cache-dir \
    --index-url https://download.pytorch.org/whl/cpu \
    torch torchvision
  python3 -m pip install --break-system-packages --no-cache-dir ultralytics onnx
  tmp_export="$(mktemp -d)"
  (
    cd "${tmp_export}"
    yolo export model=yolov8n.pt format=onnx opset=12 imgsz=640 simplify=False
  )
  cp "${tmp_export}/yolov8n.onnx" "${model_dir}/yolov8n.onnx"
fi

cmake -S "${demo_dir}" -B "${build_dir}" \
  -DCMAKE_BUILD_TYPE=Release \
  -DCMAKE_SYSTEM_NAME=Linux \
  -DCMAKE_SYSTEM_PROCESSOR=aarch64 \
  -DCMAKE_C_COMPILER=aarch64-linux-gnu-gcc \
  -DCMAKE_CXX_COMPILER=aarch64-linux-gnu-g++ \
  -DONNXRUNTIME_ROOT="/work/${third_party}/onnxruntime" \
  -DCMAKE_INSTALL_PREFIX="${install_dir}"
cmake --build "${build_dir}" -j"$(nproc)"
cmake --install "${build_dir}"

cp -a "${third_party}/onnxruntime/lib"/libonnxruntime.so* "${install_dir}/lib/"

copy_aarch64_lib() {
  local lib="$1"
  if [[ -f "${lib}" ]]; then
    cp -aL "${lib}" "${install_dir}/lib/"
  fi
}

for lib in \
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
  "$(aarch64-linux-gnu-g++ -print-file-name=libstdc++.so.6)" \
  "$(aarch64-linux-gnu-gcc -print-file-name=libgcc_s.so.1)"; do
  copy_aarch64_lib "${lib}"
done
test -f "${install_dir}/lib/ld-linux-aarch64.so.1"
test -f "${install_dir}/lib/libc.so.6"
test -f "${install_dir}/lib/libstdc++.so.6"
cp -a "${model_dir}" "${install_dir}/"
cp -a "${demo_dir}/validation" "${install_dir}/"

file "${install_dir}/aicp_yolov8_onnx_cpu"
echo "[aicp-yolo-cpu] install: ${install_dir}"
'
