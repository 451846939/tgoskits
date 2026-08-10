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


def percentile(values, pct):
    if not values:
        return 0
    values = sorted(values)
    rank = (len(values) - 1) * pct / 100.0
    lo = int(rank)
    hi = min(lo + 1, len(values) - 1)
    frac = rank - lo
    return values[lo] * (1.0 - frac) + values[hi] * frac


def main():
    if len(sys.argv) not in (2, 3):
        print(f"usage: {sys.argv[0]} latency.csv [expected_samples]", file=sys.stderr)
        return 2
    expected = int(sys.argv[2]) if len(sys.argv) == 3 else None

    rows = []
    with open(sys.argv[1], newline="") as f:
        for row in csv.DictReader(f):
            rows.append(row)

    rtts = [int(row["rtt_ns"]) for row in rows]
    errors = [abs(float(row["error"])) for row in rows]
    success = len(rows)

    print(f"samples={success}")
    if expected is not None:
        print(f"expected_samples={expected}")
        print(f"success_rate={success / expected:.6f}")
    if not rtts:
        return 1
    print(f"rtt_ns_min={min(rtts)}")
    print(f"rtt_ns_avg={int(statistics.fmean(rtts))}")
    print(f"rtt_ns_p95={int(percentile(rtts, 95))}")
    print(f"rtt_ns_p99={int(percentile(rtts, 99))}")
    print(f"rtt_ns_max={max(rtts)}")
    print(f"abs_error_avg={statistics.fmean(errors):.6f}")
    print(f"abs_error_max={max(errors):.6f}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
