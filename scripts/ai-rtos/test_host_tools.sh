#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
source "${repo_root}/scripts/ai-rtos/lib/host_tools.sh"
source "${repo_root}/scripts/ai-rtos/lib/markers.sh"
source "${repo_root}/scripts/ai-rtos/lib/process.sh"

test_dir="$(mktemp -d "${TMPDIR:-/tmp}/aicp-host-tools.XXXXXX")"
trap 'rm -rf "${test_dir}"' EXIT

fake_bin="${test_dir}/toolchain/bin"
mkdir -p "${fake_bin}"
printf '#!/usr/bin/env bash\nexit 0\n' > "${fake_bin}/aarch64-elf-gcc"
chmod +x "${fake_bin}/aarch64-elf-gcc"

expected_prefix="${fake_bin}/aarch64-elf-"

revision_key="$(aicp_revision_key 'refs/tags/v4.2.0')"
if [[ "${revision_key}" != "refs-tags-v4.2.0" ]]; then
  echo "FAIL: revision 工作区键生成错误：${revision_key}" >&2
  exit 1
fi

resolved="$(PATH="${fake_bin}:${PATH}" aicp_resolve_cross_prefix TEST_CROSS_COMPILE aarch64-elf-)"
if [[ "${resolved}" != "${expected_prefix}" ]]; then
  echo "FAIL: PATH 前缀解析结果错误：${resolved}" >&2
  exit 1
fi

TEST_CROSS_COMPILE="${expected_prefix}"
export TEST_CROSS_COMPILE
resolved="$(aicp_resolve_cross_prefix TEST_CROSS_COMPILE unused-prefix-)"
if [[ "${resolved}" != "${expected_prefix}" ]]; then
  echo "FAIL: 显式绝对前缀解析结果错误：${resolved}" >&2
  exit 1
fi

yolo_install_dir="${test_dir}/yolov8-install"
mkdir -p \
  "${yolo_install_dir}/lib" \
  "${yolo_install_dir}/model" \
  "${yolo_install_dir}/validation"
touch \
  "${yolo_install_dir}/aicp_yolov8_rust_onnx" \
  "${yolo_install_dir}/lib/ld-linux-aarch64.so.1" \
  "${yolo_install_dir}/lib/libonnxruntime.so.1.18.1" \
  "${yolo_install_dir}/model/yolov8n.onnx" \
  "${yolo_install_dir}/model/coco_80_labels_list.txt" \
  "${yolo_install_dir}/validation/images.txt" \
  "${yolo_install_dir}/validation/tennis-ball-close.jpg" \
  "${yolo_install_dir}/validation/tennis-ball-black-box.jpg" \
  "${yolo_install_dir}/validation/tennis-ball-plant.jpg"
chmod +x "${yolo_install_dir}/aicp_yolov8_rust_onnx"
if ! aicp_yolo_rust_bundle_ready "${yolo_install_dir}"; then
  echo "FAIL: 完整 YOLOv8 运行包未被识别" >&2
  exit 1
fi

TEST_CROSS_COMPILE="missing-aicp-toolchain-"
if aicp_resolve_cross_prefix TEST_CROSS_COMPILE unused-prefix- >/dev/null 2>&1; then
  echo "FAIL: 不存在的编译器前缀被错误接受" >&2
  exit 1
fi

marker_log="${test_dir}/arceos-ready.log"
printf '%s\n' 'AICP ArceOS RTOS TCP server listening on 0.0.0.0:8800' > "${marker_log}"
if ! aicp_wait_for_arceos_ready \
  "$((SECONDS + 1))" "$$" "${marker_log}" 20; then
  echo "FAIL: ArceOS TCP 监听日志未被识别为服务就绪" >&2
  exit 1
fi

terminal_failure_log="${test_dir}/terminal-failure.log"
printf '%s\n' 'AICP_RTTHREAD_RELIABILITY_SUMMARY passed=4 failed=4' > "${terminal_failure_log}"
if ! aicp_logs_have_terminal_failure "${terminal_failure_log}"; then
  echo "FAIL: failed>0 的终止汇总未被识别" >&2
  exit 1
fi

printf '%s\n' \
  'AICP_RTTHREAD_RELIABILITY_SUMMARY passed=8 failed=0' \
  'AICP_LINUX_DONE ok=20 failed=0' \
  'AICP_YOLO_RUST_DONE ok=3 failed=0' > "${terminal_failure_log}"
if aicp_logs_have_terminal_failure "${terminal_failure_log}"; then
  echo "FAIL: failed=0 的成功汇总被误判为终止失败" >&2
  exit 1
fi

host_irq_failure_log="${test_dir}/host-irq-failure.log"
printf '%s\n' \
  'Unhandled IRQ IrqId { domain: IrqDomainId(7), hwirq: HwIrq(26) } on CPU 1' \
  > "${host_irq_failure_log}"
if ! aicp_logs_have_fatal_host_irq "${host_irq_failure_log}"; then
  echo "FAIL: 宿主 EL2 物理定时器 PPI 26 未处理事件未被识别" >&2
  exit 1
fi

printf '%s\n' \
  'AICP_FREERTOS_NET_IRQ_ENABLED intid=77 priority=240' \
  'AICP_LINUX_DONE ok=1 failed=0' > "${host_irq_failure_log}"
if aicp_logs_have_fatal_host_irq "${host_irq_failure_log}"; then
  echo "FAIL: 正常的 Guest SPI 与成功汇总被误判为宿主 IRQ 故障" >&2
  exit 1
fi

image_lock_bin="${test_dir}/image-lock-bin"
mkdir -p "${image_lock_bin}"
cat > "${image_lock_bin}/lsof" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' 101 202
EOF
cat > "${image_lock_bin}/ps" <<'EOF'
#!/usr/bin/env bash
case "$2" in
  101) printf '%s\n' '/usr/libexec/file-indexer' ;;
  202) printf '%s\n' '/tools/qemu-system-aarch64' ;;
esac
EOF
chmod +x "${image_lock_bin}/lsof" "${image_lock_bin}/ps"
touch "${test_dir}/rootfs.img"
qemu_users="$(PATH="${image_lock_bin}:${PATH}" aicp_image_qemu_users "${test_dir}/rootfs.img")"
if [[ "${qemu_users}" != "202 /tools/qemu-system-aarch64" ]]; then
  echo "FAIL: 镜像占用检查未过滤非 QEMU 只读访问：${qemu_users}" >&2
  exit 1
fi

child_pid_file="${test_dir}/stubborn-child.pid"
(
  aicp_exec_new_session bash -c \
    'trap "" TERM; trap "kill -KILL ${child_pid:-} 2>/dev/null || true" EXIT; sleep 300 & child_pid=$!; printf "%s\n" "$child_pid" > "$1"; wait' \
    bash "${child_pid_file}"
) &
process_root_pid=$!
for _ in $(seq 1 50); do
  [[ -s "${child_pid_file}" ]] && break
  sleep 0.02
done
if [[ ! -s "${child_pid_file}" ]]; then
  echo "FAIL: 未能创建进程树清理测试子进程" >&2
  exit 1
fi
process_child_pid="$(cat "${child_pid_file}")"
aicp_cleanup_process_tree "${process_root_pid}"

# kill -0 reports success for a zombie until its parent reaps it. The cleanup
# contract is that no process remains executable, so treat a zombie as exited.
aicp_process_is_live() {
  local pid="$1"
  local state

  kill -0 "${pid}" 2>/dev/null || return 1
  state="$(ps -p "${pid}" -o stat= 2>/dev/null || true)"
  [[ -n "${state}" && "${state}" != Z* ]]
}

if aicp_process_is_live "${process_root_pid}" || aicp_process_is_live "${process_child_pid}"; then
  echo "FAIL: 进程树清理后仍有测试进程存活" >&2
  exit 1
fi

echo "PASS: host tool resolution and marker detection"
