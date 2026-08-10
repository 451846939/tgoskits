#!/usr/bin/env python3
# Copyright 2026 The TGOSKits Authors
#
# SPDX-License-Identifier: Apache-2.0

import argparse
from pathlib import Path
from typing import Dict, Optional


KEYS = (
    "rtt_ns_avg",
    "rtt_ns_p99",
    "rtt_ns_max",
    "rtos_service_ns_avg",
    "rtos_service_ns_p99",
    "rtos_service_ns_max",
    "rtos_wake_lateness_ns_avg",
    "rtos_wake_lateness_ns_p99",
    "rtos_wake_lateness_ns_max",
    "rtos_interval_abs_jitter_ns_avg",
    "rtos_interval_abs_jitter_ns_p99",
    "rtos_interval_abs_jitter_ns_max",
    "rtos_missed_deadlines",
    "request_interval_abs_deviation_ns_avg",
    "request_interval_abs_deviation_ns_p99",
    "request_interval_abs_deviation_ns_max",
)


def load_summary(path: Path) -> Dict[str, float]:
    data: Dict[str, float] = {}
    if not path.is_file():
        return data
    for line in path.read_text(errors="replace").splitlines():
        if "=" not in line:
            continue
        key, value = line.split("=", 1)
        try:
            data[key] = float(value)
        except ValueError:
            pass
    return data


def fmt(value: Optional[float]) -> str:
    if value is None:
        return "n/a"
    if value.is_integer():
        return str(int(value))
    return f"{value:.6f}"


def improvement(before: Optional[float], after: Optional[float]) -> str:
    if before is None or after is None or before == 0:
        return "n/a"
    return f"{(before - after) / before * 100.0:.2f}%"


def load_variant_status(root: Path) -> str:
    path = root / "variant.status.txt"
    if not path.is_file():
        return "UNKNOWN"
    return "PASS" if path.read_text(errors="replace").startswith("status=PASS") else "FAIL"


def main() -> int:
    parser = argparse.ArgumentParser(description="Summarize AxVisor RT before/after matrix output.")
    parser.add_argument("before_dir", type=Path)
    parser.add_argument("after_dir", type=Path)
    parser.add_argument("--summary", type=Path, required=True)
    args = parser.parse_args()

    lines = [
        f"before_dir={args.before_dir}",
        f"after_dir={args.after_dir}",
        f"before_status={load_variant_status(args.before_dir)}",
        f"after_status={load_variant_status(args.after_dir)}",
    ]
    for case in ("idle", "stress"):
        before = load_summary(args.before_dir / f"{case}.summary.txt")
        after = load_summary(args.after_dir / f"{case}.summary.txt")
        lines.append("")
        lines.append(f"[{case}]")
        for key in KEYS:
            before_value = before.get(key)
            after_value = after.get(key)
            lines.append(
                f"{key}: before={fmt(before_value)} after={fmt(after_value)} "
                f"improvement={improvement(before_value, after_value)}"
            )

    args.summary.write_text("\n".join(lines) + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
