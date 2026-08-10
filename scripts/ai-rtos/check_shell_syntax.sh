#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
#
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

find "${repo_root}/scripts/ai-rtos" -type f -name '*.sh' -print0 \
  | xargs -0 -n 1 bash -n

echo "[ai-rtos] PASS: shell syntax checked"
