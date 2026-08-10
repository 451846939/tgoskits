#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
#
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
demo_dir="$repo_root/apps/ai-rtos-demo"
port="${AICP_PORT:-18800}"
iterations="${AICP_ITERATIONS:-50}"
mode="${AICP_MODE:-ai}"
csv="${AICP_CSV:-$demo_dir/build/aicp_latency.csv}"
summary="${AICP_SUMMARY:-$demo_dir/build/aicp_summary.txt}"

make -C "$demo_dir" all

"$demo_dir/build/aicp_server" "$port" >"$demo_dir/build/aicp_server.log" 2>&1 &
server_pid=$!
trap 'kill "$server_pid" 2>/dev/null || true' EXIT

sleep 0.2
"$demo_dir/build/aicp_client" 127.0.0.1 "$port" "$iterations" "$csv" "$mode"
"$repo_root/scripts/ai-rtos/summarize_latency.py" "$csv" "$iterations" | tee "$summary"
