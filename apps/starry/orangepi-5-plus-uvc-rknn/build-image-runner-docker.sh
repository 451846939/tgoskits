#!/usr/bin/env bash
set -euo pipefail

case_dir="$(cd "$(dirname "$0")" && pwd)"
repo_root="$(cd "${case_dir}/../../.." && pwd)"
image="${RKNN_YOLOV8_DOCKER_IMAGE:-clion-ubuntu:24.04}"

if ! docker image inspect "${image}" >/dev/null 2>&1; then
  image="ubuntu:24.04"
fi

docker run --rm \
  -v "${repo_root}:/work" \
  -w /work \
  "${image}" \
  bash -lc '
    set -euo pipefail
    if ! command -v aarch64-linux-gnu-g++ >/dev/null 2>&1; then
      export DEBIAN_FRONTEND=noninteractive
      apt-get update
      apt-get install -y --no-install-recommends \
        ca-certificates \
        cmake \
        make \
        gcc-aarch64-linux-gnu \
        g++-aarch64-linux-gnu
    fi
    CROSS_COMPILE=aarch64-linux-gnu- \
      apps/starry/orangepi-5-plus-uvc-rknn/build-image-runner.sh
  '
