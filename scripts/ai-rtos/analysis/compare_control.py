#!/usr/bin/env python3
# Copyright 2026 The TGOSKits Authors
#
# SPDX-License-Identifier: Apache-2.0

# Copyright 2026 The TGOSKits Authors
#
# Licensed under the Apache License, Version 2.0.

import csv
import statistics
import sys


def read_rows(path):
    with open(path, newline="") as f:
        return list(csv.DictReader(f))


def metrics(rows, expected):
    rtts = [int(row["rtt_ns"]) for row in rows]
    errors = [abs(float(row["error"])) for row in rows]
    outputs = [
        abs(float(row["control_output"]))
        for row in rows
        if row.get("control_output")
    ]
    return {
        "samples": len(rows),
        "success_rate": len(rows) / expected if expected else 0.0,
        "rtt_avg_ns": int(statistics.fmean(rtts)) if rtts else 0,
        "rtt_max_ns": max(rtts) if rtts else 0,
        "abs_error_avg": statistics.fmean(errors) if errors else 0.0,
        "abs_error_max": max(errors) if errors else 0.0,
        "control_output_avg_abs": statistics.fmean(outputs) if outputs else 0.0,
        "control_output_samples": len(outputs),
    }


def emit(prefix, data):
    for key, value in data.items():
        if isinstance(value, float):
            print(f"{prefix}_{key}={value:.6f}")
        else:
            print(f"{prefix}_{key}={value}")


def main():
    if len(sys.argv) != 4:
        print(f"usage: {sys.argv[0]} fixed.csv ai.csv expected_samples", file=sys.stderr)
        return 2

    expected = int(sys.argv[3])
    fixed = metrics(read_rows(sys.argv[1]), expected)
    ai = metrics(read_rows(sys.argv[2]), expected)
    emit("fixed", fixed)
    emit("ai", ai)
    if fixed["abs_error_avg"] > 0:
        improvement = (fixed["abs_error_avg"] - ai["abs_error_avg"]) / fixed["abs_error_avg"]
        print(f"abs_error_avg_improvement={improvement:.6f}")
    if fixed["rtt_avg_ns"] > 0:
        rtt_delta = ai["rtt_avg_ns"] - fixed["rtt_avg_ns"]
        print(f"rtt_avg_delta_ns={rtt_delta}")
    return 0 if fixed["samples"] and ai["samples"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
