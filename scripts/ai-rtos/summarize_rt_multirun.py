#!/usr/bin/env python3
# Copyright 2026 The TGOSKits Authors
#
# SPDX-License-Identifier: Apache-2.0

import argparse
import statistics
from pathlib import Path
from typing import Optional


METRICS = (
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
    "host_irq_handler_avg_ns",
    "host_irq_handler_max_ns",
)


def load(path: Path) -> dict[str, float]:
    values: dict[str, float] = {}
    for line in path.read_text(errors="replace").splitlines():
        if "=" not in line:
            continue
        key, value = line.split("=", 1)
        try:
            values[key] = float(value)
        except ValueError:
            pass
    return values


def summary_path(root: Path, variant: str, case: str) -> Optional[Path]:
    path = root / variant / f"{case}.summary.txt"
    if path.is_file():
        return path
    if variant == "baseline":
        for legacy_variant in ("shared-wait-baseline", "compat"):
            legacy = root / legacy_variant / f"{case}.summary.txt"
            if legacy.is_file():
                return legacy
    return None


def load_round_metric(
    root: Path, variant: str, case: str, metric: str
) -> Optional[float]:
    path = summary_path(root, variant, case)
    if path is None:
        return None
    return load(path).get(metric)


def main() -> int:
    parser = argparse.ArgumentParser(description="汇总多轮 AxVisor 实时 A/B 结果")
    parser.add_argument("result_dirs", nargs="+", type=Path)
    parser.add_argument("--summary", required=True, type=Path)
    args = parser.parse_args()

    lines = [f"rounds={len(args.result_dirs)}"]
    for case in ("idle", "stress"):
        lines.append("")
        lines.append(f"[{case}]")
        for metric in METRICS:
            before = [
                load_round_metric(root, "baseline", case, metric)
                for root in args.result_dirs
            ]
            after = [
                load_round_metric(root, "optimized", case, metric)
                for root in args.result_dirs
            ]
            before_values = [value for value in before if value is not None]
            after_values = [value for value in after if value is not None]
            if not before_values or not after_values:
                continue
            before_median = statistics.median(before_values)
            after_median = statistics.median(after_values)
            before_worst = max(before_values)
            after_worst = max(after_values)
            median_improvement = (
                (before_median - after_median) / before_median * 100.0
                if before_median
                else 0.0
            )
            worst_improvement = (
                (before_worst - after_worst) / before_worst * 100.0
                if before_worst
                else 0.0
            )
            lines.append(
                f"{metric}: before_median={before_median:.0f} "
                f"before_samples={len(before_values)} "
                f"before_range={min(before_values):.0f}..{before_worst:.0f} "
                f"after_median={after_median:.0f} "
                f"after_samples={len(after_values)} "
                f"after_range={min(after_values):.0f}..{after_worst:.0f} "
                f"median_improvement={median_improvement:.2f}% "
                f"worst_improvement={worst_improvement:.2f}%"
            )

    args.summary.parent.mkdir(parents=True, exist_ok=True)
    args.summary.write_text("\n".join(lines) + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
