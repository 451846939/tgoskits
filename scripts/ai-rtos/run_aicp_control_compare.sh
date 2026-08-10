#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
#
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
demo_dir="$repo_root/apps/ai-rtos-demo"
port="${AICP_PORT:-18801}"
iterations="${AICP_ITERATIONS:-100}"
fixed_csv="${AICP_FIXED_CSV:-$demo_dir/build/aicp_fixed_latency.csv}"
ai_csv="${AICP_AI_CSV:-$demo_dir/build/aicp_ai_latency.csv}"
summary="${AICP_COMPARE_SUMMARY:-$demo_dir/build/aicp_control_compare.txt}"

make -C "$demo_dir" all

"$demo_dir/build/aicp_server" "$port" >"$demo_dir/build/aicp_compare_server.log" 2>&1 &
server_pid=$!
trap 'kill "$server_pid" 2>/dev/null || true' EXIT

sleep 0.2
"$demo_dir/build/aicp_client" 127.0.0.1 "$port" "$iterations" "$fixed_csv" fixed
"$demo_dir/build/aicp_client" 127.0.0.1 "$port" "$iterations" "$ai_csv" ai
"$repo_root/scripts/ai-rtos/compare_control.py" "$fixed_csv" "$ai_csv" "$iterations" | tee "$summary"
