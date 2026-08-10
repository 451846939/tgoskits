#!/usr/bin/env python3
# Copyright 2026 The TGOSKits Authors
#
# SPDX-License-Identifier: Apache-2.0

# Copyright 2026 The TGOSKits Authors
#
# Licensed under the Apache License, Version 2.0.

import argparse
import csv
import os
import statistics
import time


def percentile(values, pct):
    values = sorted(values)
    if not values:
        return 0
    rank = (len(values) - 1) * pct / 100.0
    lo = int(rank)
    hi = min(lo + 1, len(values) - 1)
    return values[lo] + (values[hi] - values[lo]) * (rank - lo)


def main():
    parser = argparse.ArgumentParser(description="Measure periodic task wake-up latency.")
    parser.add_argument("--period-us", type=int, default=1000)
    parser.add_argument("--samples", type=int, default=60000)
    parser.add_argument("--csv", default="periodic_latency.csv")
    args = parser.parse_args()

    period_ns = args.period_us * 1000
    next_deadline = time.monotonic_ns() + period_ns
    latencies = []

    with open(args.csv, "w", newline="") as f:
        writer = csv.writer(f)
        writer.writerow(["sample", "deadline_ns", "actual_ns", "latency_ns"])
        for i in range(args.samples):
            now = time.monotonic_ns()
            sleep_ns = next_deadline - now
            if sleep_ns > 0:
                time.sleep(sleep_ns / 1_000_000_000)
            actual = time.monotonic_ns()
            latency = actual - next_deadline
            latencies.append(latency)
            writer.writerow([i, next_deadline, actual, latency])
            next_deadline += period_ns

    print(f"pid={os.getpid()} samples={len(latencies)} period_us={args.period_us}")
    print(f"latency_ns_min={min(latencies)}")
    print(f"latency_ns_avg={int(statistics.fmean(latencies))}")
    print(f"latency_ns_p95={int(percentile(latencies, 95))}")
    print(f"latency_ns_p99={int(percentile(latencies, 99))}")
    print(f"latency_ns_max={max(latencies)}")


if __name__ == "__main__":
    main()
