#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
workspace_root="$(cd "${script_dir}/../../.." && pwd)"

zephyr_base="${ZEPHYR_BASE:-}"
build_dir="${AXVISOR_ZEPHYR_IVC_BUILD_DIR:-${workspace_root}/tmp/axbuild/zephyr/ivc_publisher}"
out_dir="${AXVISOR_ZEPHYR_IVC_OUT_DIR:-${build_dir}/out}"
board="${AXVISOR_ZEPHYR_IVC_BOARD:-qemu_cortex_a53}"
image_name="${AXVISOR_ZEPHYR_IVC_IMAGE_NAME:-zephyr-ivc-publisher.bin}"
zephyr_python="${ZEPHYR_PYTHON:-}"
zephyr_pyenv="${AXVISOR_ZEPHYR_IVC_PYENV:-${workspace_root}/tmp/axbuild/zephyr/pyenv}"
cross_compile="${CROSS_COMPILE:-}"
zephyr_repo_url="${ZEPHYR_REPO_URL:-https://github.com/zephyrproject-rtos/zephyr.git}"
zephyr_ref="${ZEPHYR_REF:-30bef2a126198f73ecc1f8a90590579e03379b18}"
zephyr_src_dir="${ZEPHYR_SRC_DIR:-${workspace_root}/tmp/axbuild/zephyr/src}"
zephyr_sdk_url="${ZEPHYR_SDK_URL:-https://github.com/zephyrproject-rtos/sdk-ng/releases/download/v1.0.1/toolchain_gnu_linux-x86_64_aarch64-zephyr-elf.tar.xz}"
zephyr_sdk_dir="${ZEPHYR_SDK_DIR:-${workspace_root}/tmp/axbuild/zephyr/sdk}"

usage() {
    cat <<'USAGE'
Usage:
  ZEPHYR_BASE=/path/to/zephyr \
  apps/zephyr/ivc_publisher/build.sh

Environment:
  ZEPHYR_BASE                    Optional Zephyr source tree. If omitted, the
                                 script prepares one under tmp/axbuild/zephyr.
  ZEPHYR_REPO_URL                Defaults to upstream Zephyr.
  ZEPHYR_REF                     Defaults to the ref used by tgosimages.
  ZEPHYR_SRC_DIR                 Defaults to tmp/axbuild/zephyr/src.
  ZEPHYR_SDK_URL                 Defaults to the aarch64 Zephyr SDK archive.
  ZEPHYR_SDK_DIR                 Defaults to tmp/axbuild/zephyr/sdk.
  ZEPHYR_TOOLCHAIN_VARIANT       Defaults to cross-compile.
  CROSS_COMPILE                  Optional aarch64-zephyr-elf- prefix. If omitted,
                                 the script prepares one under tmp/axbuild/zephyr.
  AXVISOR_ZEPHYR_IVC_BOARD       Defaults to qemu_cortex_a53.
  AXVISOR_ZEPHYR_IVC_BUILD_DIR   Defaults to tmp/axbuild/zephyr/ivc_publisher.
  AXVISOR_ZEPHYR_IVC_OUT_DIR     Defaults to <build-dir>/out.
  AXVISOR_ZEPHYR_IVC_IMAGE_NAME  Defaults to zephyr-ivc-publisher.bin.
  ZEPHYR_PYTHON                  Optional Python with Zephyr requirements.
USAGE
}

case "${1:-}" in
    -h|--help|help)
        usage
        exit 0
        ;;
esac

prepare_zephyr_source() {
    if [[ -n "${zephyr_base}" ]]; then
        if [[ ! -d "${zephyr_base}/cmake" || ! -d "${zephyr_base}/include/zephyr" ]]; then
            echo "invalid ZEPHYR_BASE: ${zephyr_base}" >&2
            exit 2
        fi
        zephyr_base="$(cd "${zephyr_base}" && pwd)"
        return 0
    fi

    if [[ ! -d "${zephyr_src_dir}/.git" ]]; then
        mkdir -p "$(dirname "${zephyr_src_dir}")"
        git clone "${zephyr_repo_url}" "${zephyr_src_dir}"
    fi

    if [[ -n "${zephyr_ref}" ]]; then
        git -C "${zephyr_src_dir}" fetch --depth 1 origin "${zephyr_ref}" || \
            git -C "${zephyr_src_dir}" fetch origin "${zephyr_ref}"
        git -C "${zephyr_src_dir}" checkout --detach FETCH_HEAD
    fi

    if [[ ! -d "${zephyr_src_dir}/cmake" || ! -d "${zephyr_src_dir}/include/zephyr" ]]; then
        echo "failed to prepare Zephyr source at ${zephyr_src_dir}" >&2
        exit 2
    fi

    zephyr_base="$(cd "${zephyr_src_dir}" && pwd)"
}

prepare_zephyr_source

if [[ -z "${zephyr_python}" ]]; then
    for candidate in "${zephyr_pyenv}/bin/python"; do
        if [[ -x "${candidate}" ]]; then
            zephyr_python="${candidate}"
            break
        fi
    done
fi

python_has_zephyr_requirements() {
    local python="$1"
    [[ -x "${python}" ]] && "${python}" -c 'import pykwalify.core, yaml, west' >/dev/null 2>&1
}

if [[ -n "${zephyr_python}" ]] && ! python_has_zephyr_requirements "${zephyr_python}"; then
    zephyr_python=""
fi

if [[ -z "${zephyr_python}" ]] && python_has_zephyr_requirements python3; then
    zephyr_python="$(command -v python3)"
fi

if [[ -z "${zephyr_python}" ]]; then
    python3 -m venv "${zephyr_pyenv}"
    "${zephyr_pyenv}/bin/pip" install --upgrade pip
    "${zephyr_pyenv}/bin/pip" install -r "${zephyr_base}/scripts/requirements-base.txt"
    zephyr_python="${zephyr_pyenv}/bin/python"
fi

if ! python_has_zephyr_requirements "${zephyr_python}"; then
    echo "failed to prepare Python with Zephyr build requirements" >&2
    exit 2
fi

export ZEPHYR_BASE="${zephyr_base}"
export ZEPHYR_TOOLCHAIN_VARIANT="${ZEPHYR_TOOLCHAIN_VARIANT:-cross-compile}"

if [[ -n "${cross_compile}" ]] && ! command -v "${cross_compile}gcc" >/dev/null 2>&1; then
    cross_compile=""
fi

if [[ -z "${cross_compile}" ]]; then
    for candidate in "${zephyr_sdk_dir}/bin/aarch64-zephyr-elf-" aarch64-zephyr-elf-; do
        if command -v "${candidate}gcc" >/dev/null 2>&1; then
            cross_compile="${candidate}"
            break
        fi
    done
fi

if [[ -z "${cross_compile}" ]]; then
    tmpdir="$(mktemp -d)"
    archive="${tmpdir}/zephyr-sdk.tar.xz"
    curl -fSL -o "${archive}" "${zephyr_sdk_url}"
    mkdir -p "${zephyr_sdk_dir}"
    tar xf "${archive}" -C "${zephyr_sdk_dir}" --strip-components=1
    rm -rf "${tmpdir}"
    cross_compile="${zephyr_sdk_dir}/bin/aarch64-zephyr-elf-"
fi

if [[ -z "${cross_compile}" || ! -x "$(command -v "${cross_compile}gcc" 2>/dev/null)" ]]; then
    echo "aarch64 Zephyr toolchain not found; set CROSS_COMPILE=/path/to/aarch64-zephyr-elf-" >&2
    exit 2
fi

export CROSS_COMPILE="${cross_compile}"

reset_stale_build_cache() {
    local cache="${build_dir}/CMakeCache.txt"
    [[ -f "${cache}" ]] || return 0

    local cached_home cached_compiler cached_board cached_python
    cached_home="$(sed -n 's/^CMAKE_HOME_DIRECTORY:INTERNAL=//p' "${cache}" | tail -n 1)"
    cached_compiler="$(sed -n 's/^CMAKE_C_COMPILER:FILEPATH=//p' "${cache}" | tail -n 1)"
    cached_board="$(sed -n 's/^BOARD:STRING=//p' "${cache}" | tail -n 1)"
    cached_python="$(sed -n 's/^Python3_EXECUTABLE:FILEPATH=//p' "${cache}" | tail -n 1)"

    if [[ "${cached_home}" != "${script_dir}" ||
          "${cached_compiler}" != "${cross_compile}gcc" ||
          "${cached_board}" != "${board}" ||
          "${cached_python}" != "${zephyr_python}" ]]; then
        echo "Resetting stale Zephyr IVC build cache: ${build_dir}"
        rm -rf "${build_dir}"
    fi
}

reset_stale_build_cache

prepare_module_metadata() {
    mkdir -p "${build_dir}/Kconfig"

    "${zephyr_python}" "${zephyr_base}/scripts/zephyr_module.py" \
        --kconfig-out "${build_dir}/Kconfig/Kconfig.modules" \
        --sysbuild-kconfig-out "${build_dir}/Kconfig/Kconfig.sysbuild.modules" \
        --cmake-out "${build_dir}/zephyr_modules.txt" \
        --settings-out "${build_dir}/zephyr_settings.txt" \
        -z "${zephyr_base}"
}

prepare_module_metadata

mkdir -p "${out_dir}"

cmake_args=(
    -GNinja \
    -U Python3_EXECUTABLE \
    -U CROSS_COMPILE \
    -B "${build_dir}" \
    -S "${script_dir}" \
    -DBOARD="${board}" \
    -DZEPHYR_TOOLCHAIN_VARIANT="${ZEPHYR_TOOLCHAIN_VARIANT}" \
    -DPython3_EXECUTABLE="${zephyr_python}" \
    -DCROSS_COMPILE="${cross_compile}"
)

cmake "${cmake_args[@]}"

cmake --build "${build_dir}" -j"$(nproc)"

cp -f "${build_dir}/zephyr/zephyr.bin" "${out_dir}/${image_name}"
if [[ -f "${build_dir}/zephyr/zephyr.elf" ]]; then
    cp -f "${build_dir}/zephyr/zephyr.elf" "${out_dir}/${image_name%.bin}.elf"
fi

echo "AXVISOR_ZEPHYR_IVC_OUT_DIR=${out_dir}"
