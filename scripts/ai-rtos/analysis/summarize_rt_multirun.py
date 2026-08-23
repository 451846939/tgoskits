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
    if variant == "control":
        for legacy_variant in ("baseline", "shared-wait-baseline", "compat"):
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
                load_round_metric(root, "control", case, metric)
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
            control_median = statistics.median(before_values)
            optimized_median = statistics.median(after_values)
            control_worst = max(before_values)
            optimized_worst = max(after_values)
            median_improvement = (
                (control_median - optimized_median) / control_median * 100.0
                if control_median
                else 0.0
            )
            worst_improvement = (
                (control_worst - optimized_worst) / control_worst * 100.0
                if control_worst
                else 0.0
            )
            lines.append(
                f"{metric}: control_median={control_median:.0f} "
                f"control_samples={len(before_values)} "
                f"control_range={min(before_values):.0f}..{control_worst:.0f} "
                f"optimized_median={optimized_median:.0f} "
                f"optimized_samples={len(after_values)} "
                f"optimized_range={min(after_values):.0f}..{optimized_worst:.0f} "
                f"median_improvement={median_improvement:.2f}% "
                f"worst_improvement={worst_improvement:.2f}%"
            )

    args.summary.parent.mkdir(parents=True, exist_ok=True)
    args.summary.write_text("\n".join(lines) + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
