#!/usr/bin/env python3
# Copyright 2026 The TGOSKits Authors
#
# SPDX-License-Identifier: Apache-2.0

import argparse
import csv
import re
import statistics


ANSI_CSI_RE = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]")
INTERLEAVED_LINUX_STATUS_RE = re.compile(r"\bA?ICP_LIm?NUX_STATUS\b")
STATUS_RE = re.compile(
    r"AICP_(?:LINUX|STARRY)_STATUS\s+seq=(?P<seq>\d+)\s+target=(?P<target>-?\d+\.\d+)\s+"
    r"measured=(?P<measured>-?\d+\.\d+)\s+error=(?P<error>-?\d+\.\d+)\s+"
    r"rtt_ns=(?P<rtt_ns>\d+)"
)
DONE_RE = re.compile(
    r"AICP_(?:LINUX|STARRY)_DONE\s+ok=(?P<ok>\d+)\s+failed=(?P<failed>\d+)\s+"
    r"avg_rtt_ns=(?P<avg>\d+)\s+max_rtt_ns=(?P<max>\d+)"
)
RT_TRACE_RE = re.compile(r"rt-trace: (?P<body>.*)")
IRQ_DISPATCH_RE = re.compile(
    r"interrupt dispatch total=(?P<count>\d+).*?avg_ns=(?P<avg>\d+).*?max_ns=(?P<max>\d+)"
)
VCPU_RESUME_RE = re.compile(
    r"vcpu resume count=(?P<count>\d+).*?avg_ns=(?P<avg>\d+).*?max_ns=(?P<max>\d+)"
)
HOST_IRQ_DISPATCH_RE = re.compile(
    r"host irq dispatch count=(?P<count>\d+).*?avg_ns=(?P<avg>\d+).*?max_ns=(?P<max>\d+)"
)
HOST_IRQ_HANDLER_RE = re.compile(
    r"host irq handler count=(?P<count>\d+).*?avg_ns=(?P<avg>\d+).*?max_ns=(?P<max>\d+)"
)
CONTROL_RE = re.compile(
    r"CONTROL\s+seq=(?P<seq>\d+)\s+target=(?P<target>-?\d+\.\d+)\s+"
    r"measured=(?P<measured>-?\d+\.\d+)\s+output=(?P<output>-?\d+\.\d+)"
)
RTOS_REQUEST_TIMING_RE = re.compile(
    r"AICP_RTOS_REQUEST_TIMING\s+seq=(?P<seq>\d+)\s+service_ns=(?P<service>-?\d+)\s+"
    r"request_interval_ns=(?P<inter>-?\d+)\s+"
    r"request_interval_deviation_ns=(?P<deviation>-?\d+)"
)
LEGACY_RTOS_TIMING_RE = re.compile(
    r"AICP_RTOS_TIMING\s+seq=(?P<seq>\d+)\s+service_ns=(?P<service>-?\d+)\s+"
    r"inter_arrival_ns=(?P<inter>-?\d+)\s+jitter_ns=(?P<jitter>-?\d+)"
)
RTOS_PERIODIC_DONE_RE = re.compile(
    r"AICP_RTOS_PERIODIC_DONE\s+samples=(?P<samples>\d+)\s+period_ns=(?P<period>\d+)\s+"
    r"wake_lateness_avg_ns=(?P<wake_avg>\d+)\s+"
    r"wake_lateness_p99_ns=(?P<wake_p99>\d+)\s+"
    r"wake_lateness_max_ns=(?P<wake_max>\d+)\s+"
    r"interval_abs_jitter_avg_ns=(?P<interval_avg>\d+)\s+"
    r"interval_abs_jitter_p99_ns=(?P<interval_p99>\d+)\s+"
    r"interval_abs_jitter_max_ns=(?P<interval_max>\d+)\s+"
    r"missed_deadlines=(?P<missed>\d+)"
)
LINUX_CPU_RE = re.compile(
    r"AICP_LINUX_CPU\s+cpu=(?P<cpu>\d+)\s+busy_ticks=(?P<busy>\d+)\s+"
    r"total_ticks=(?P<total>\d+)\s+usage_permille=(?P<usage>\d+)"
)
LINUX_RUNTIME_RE = re.compile(
    r"AICP_LINUX_RUNTIME\s+duration_ns=(?P<duration>\d+)\s+"
    r"iterations=(?P<iterations>\d+)\s+stress_procs=(?P<stress>\d+)"
)
RTTHREAD_STATS_RE = re.compile(
    r"AICP_RTTHREAD_STATS\s+reason=(?P<reason>\w+)\s+"
    r"clients=(?P<clients>\d+)\s+disconnects=(?P<disconnects>\d+)\s+"
    r"controls=(?P<controls>\d+)\s+errors=(?P<errors>\d+)\s+"
    r"duplicates=(?P<duplicates>\d+)\s+stale=(?P<stale>\d+)\s+"
    r"irq=(?P<irq>\d+)\s+rx_frames=(?P<rx_frames>\d+)\s+"
    r"tx_frames=(?P<tx_frames>\d+)"
)


def percentile(values, pct):
    if not values:
        return 0
    values = sorted(values)
    rank = (len(values) - 1) * pct / 100.0
    lo = int(rank)
    hi = min(lo + 1, len(values) - 1)
    frac = rank - lo
    return int(values[lo] * (1.0 - frac) + values[hi] * frac)


def parse_log(path):
    rows = []
    done = None
    rt_trace = []
    rtos_request_timing = []
    rtos_periodic = None
    control_outputs = {}
    linux_cpus = {}
    linux_runtime = None
    rtthread_stats = None
    reliability = {
        "timeouts": 0,
        "fault_drops": 0,
        "duplicate_replays": 0,
        "error_notifications": 0,
        "reconnects": 0,
    }
    with open(path, errors="replace") as f:
        for raw_line in f:
            # AxVisor and guest serial output share one QEMU stream. A host-side
            # colored log record can therefore insert a CSI sequence in the
            # middle of a guest marker (for example AICP_LI<CSI m>NUX_STATUS).
            # Strip terminal controls before matching while keeping the strict
            # expected-sample check below.
            line = ANSI_CSI_RE.sub("", raw_line)
            line = INTERLEAVED_LINUX_STATUS_RE.sub("AICP_LINUX_STATUS", line)
            if match := CONTROL_RE.search(line):
                control_outputs[int(match.group("seq"))] = float(match.group("output"))
            if match := RTOS_REQUEST_TIMING_RE.search(line):
                rtos_request_timing.append(
                    {
                        "seq": int(match.group("seq")),
                        "service_ns": int(match.group("service")),
                        "request_interval_ns": int(match.group("inter")),
                        "request_interval_deviation_ns": int(match.group("deviation")),
                    }
                )
            elif match := LEGACY_RTOS_TIMING_RE.search(line):
                rtos_request_timing.append(
                    {
                        "seq": int(match.group("seq")),
                        "service_ns": int(match.group("service")),
                        "request_interval_ns": int(match.group("inter")),
                        "request_interval_deviation_ns": int(match.group("jitter")),
                    }
                )
            if match := RTOS_PERIODIC_DONE_RE.search(line):
                rtos_periodic = {
                    key: int(value) for key, value in match.groupdict().items()
                }
            if match := LINUX_CPU_RE.search(line):
                cpu = int(match.group("cpu"))
                linux_cpus[cpu] = {
                    "busy_ticks": int(match.group("busy")),
                    "total_ticks": int(match.group("total")),
                    "usage_permille": int(match.group("usage")),
                }
            if match := LINUX_RUNTIME_RE.search(line):
                linux_runtime = {
                    "duration_ns": int(match.group("duration")),
                    "iterations": int(match.group("iterations")),
                    "stress_procs": int(match.group("stress")),
                }
            if match := RTTHREAD_STATS_RE.search(line):
                rtthread_stats = {
                    key: value if key == "reason" else int(value)
                    for key, value in match.groupdict().items()
                }
            if "errno=110" in line or "AICP_TIMEOUT" in line:
                reliability["timeouts"] += 1
            if "AICP UDP fault_drop" in line:
                reliability["fault_drops"] += 1
            if "AICP UDP duplicate" in line:
                reliability["duplicate_replays"] += 1
            if "AICP_ERROR" in line or "ERROR_NOTIFICATION" in line:
                reliability["error_notifications"] += 1
            if "reconnect" in line.lower():
                reliability["reconnects"] += 1
            if match := STATUS_RE.search(line):
                row = match.groupdict()
                seq = int(row["seq"])
                rows.append(
                    {
                        "seq": seq,
                        "rtt_ns": int(row["rtt_ns"]),
                        "target": float(row["target"]),
                        "measured": float(row["measured"]),
                        "error": float(row["error"]),
                        "control_output": control_outputs.get(seq, ""),
                    }
                )
            if match := DONE_RE.search(line):
                done = {key: int(value) for key, value in match.groupdict().items()}
            if match := RT_TRACE_RE.search(line):
                rt_trace.append(match.group("body").strip())
    return (
        rows,
        done,
        rt_trace,
        rtos_request_timing,
        rtos_periodic,
        linux_cpus,
        linux_runtime,
        rtthread_stats,
        reliability,
    )


def write_csv(path, rows):
    with open(path, "w", newline="") as f:
        writer = csv.DictWriter(
            f,
            fieldnames=["seq", "rtt_ns", "target", "measured", "error", "control_output"],
        )
        writer.writeheader()
        writer.writerows(rows)


def write_summary(
    path,
    rows,
    done,
    rt_trace,
    rtos_request_timing,
    rtos_periodic,
    linux_cpus,
    linux_runtime,
    rtthread_stats,
    reliability,
    expected,
):
    rtts = [row["rtt_ns"] for row in rows]
    errors = [abs(row["error"]) for row in rows]
    service_times = [row["service_ns"] for row in rtos_request_timing]
    request_interval_deviations = [
        abs(row["request_interval_deviation_ns"])
        for row in rtos_request_timing
        if row["request_interval_ns"] > 0
    ]
    request_intervals = [
        row["request_interval_ns"]
        for row in rtos_request_timing
        if row["request_interval_ns"] > 0
    ]
    irq_dispatch = None
    vcpu_resume = None
    host_irq_dispatch = None
    host_irq_handler = None
    for line in rt_trace:
        if match := IRQ_DISPATCH_RE.search(line):
            irq_dispatch = {key: int(value) for key, value in match.groupdict().items()}
        if match := VCPU_RESUME_RE.search(line):
            vcpu_resume = {key: int(value) for key, value in match.groupdict().items()}
        if match := HOST_IRQ_DISPATCH_RE.search(line):
            host_irq_dispatch = {
                key: int(value) for key, value in match.groupdict().items()
            }
        if match := HOST_IRQ_HANDLER_RE.search(line):
            host_irq_handler = {
                key: int(value) for key, value in match.groupdict().items()
            }
    with open(path, "w") as f:
        f.write(f"samples={len(rows)}\n")
        if expected is not None:
            f.write(f"expected_samples={expected}\n")
            f.write(f"success_rate={len(rows) / expected:.6f}\n")
        if done:
            f.write(f"done_ok={done['ok']}\n")
            f.write(f"done_failed={done['failed']}\n")
            f.write(f"done_avg_rtt_ns={done['avg']}\n")
            f.write(f"done_max_rtt_ns={done['max']}\n")
        if rtts:
            f.write(f"rtt_ns_min={min(rtts)}\n")
            f.write(f"rtt_ns_avg={int(statistics.fmean(rtts))}\n")
            f.write(f"rtt_ns_p95={percentile(rtts, 95)}\n")
            f.write(f"rtt_ns_p99={percentile(rtts, 99)}\n")
            f.write(f"rtt_ns_max={max(rtts)}\n")
            f.write(f"abs_error_avg={statistics.fmean(errors):.6f}\n")
            f.write(f"abs_error_max={max(errors):.6f}\n")
        f.write(f"rtos_request_timing_samples={len(rtos_request_timing)}\n")
        if service_times:
            f.write(f"rtos_service_ns_min={min(service_times)}\n")
            f.write(f"rtos_service_ns_avg={int(statistics.fmean(service_times))}\n")
            f.write(f"rtos_service_ns_p95={percentile(service_times, 95)}\n")
            f.write(f"rtos_service_ns_p99={percentile(service_times, 99)}\n")
            f.write(f"rtos_service_ns_max={max(service_times)}\n")
        if request_intervals:
            duration_ns = sum(request_intervals)
            f.write(f"request_interval_ns_min={min(request_intervals)}\n")
            f.write(
                f"request_interval_ns_avg={int(statistics.fmean(request_intervals))}\n"
            )
            f.write(f"request_interval_ns_p95={percentile(request_intervals, 95)}\n")
            f.write(f"request_interval_ns_p99={percentile(request_intervals, 99)}\n")
            f.write(f"request_interval_ns_max={max(request_intervals)}\n")
            f.write(f"measured_duration_ns={duration_ns}\n")
            if duration_ns > 0:
                transactions_per_s = len(rows) * 1_000_000_000 / duration_ns
                f.write(f"transactions_per_s={transactions_per_s:.6f}\n")
                f.write(f"effective_payload_bytes_per_s={transactions_per_s * 48:.3f}\n")
                f.write(f"aicp_wire_bytes_per_s={transactions_per_s * 112:.3f}\n")
        if request_interval_deviations:
            f.write(
                "request_interval_abs_deviation_ns_avg="
                f"{int(statistics.fmean(request_interval_deviations))}\n"
            )
            f.write(
                "request_interval_abs_deviation_ns_p95="
                f"{percentile(request_interval_deviations, 95)}\n"
            )
            f.write(
                "request_interval_abs_deviation_ns_p99="
                f"{percentile(request_interval_deviations, 99)}\n"
            )
            f.write(
                "request_interval_abs_deviation_ns_max="
                f"{max(request_interval_deviations)}\n"
            )
        if rtos_periodic:
            f.write(f"rtos_periodic_samples={rtos_periodic['samples']}\n")
            f.write(f"rtos_periodic_target_ns={rtos_periodic['period']}\n")
            f.write(f"rtos_wake_lateness_ns_avg={rtos_periodic['wake_avg']}\n")
            f.write(f"rtos_wake_lateness_ns_p99={rtos_periodic['wake_p99']}\n")
            f.write(f"rtos_wake_lateness_ns_max={rtos_periodic['wake_max']}\n")
            f.write(
                "rtos_interval_abs_jitter_ns_avg="
                f"{rtos_periodic['interval_avg']}\n"
            )
            f.write(
                "rtos_interval_abs_jitter_ns_p99="
                f"{rtos_periodic['interval_p99']}\n"
            )
            f.write(
                "rtos_interval_abs_jitter_ns_max="
                f"{rtos_periodic['interval_max']}\n"
            )
            f.write(f"rtos_missed_deadlines={rtos_periodic['missed']}\n")
        f.write(f"rt_trace_lines={len(rt_trace)}\n")
        if irq_dispatch:
            f.write(f"irq_dispatch_count={irq_dispatch['count']}\n")
            f.write(f"irq_dispatch_avg_ns={irq_dispatch['avg']}\n")
            f.write(f"irq_dispatch_max_ns={irq_dispatch['max']}\n")
        if vcpu_resume:
            f.write(f"vcpu_resume_count={vcpu_resume['count']}\n")
            f.write(f"vcpu_resume_avg_ns={vcpu_resume['avg']}\n")
            f.write(f"vcpu_resume_max_ns={vcpu_resume['max']}\n")
        if host_irq_dispatch:
            f.write(f"host_irq_dispatch_count={host_irq_dispatch['count']}\n")
            f.write(f"host_irq_dispatch_avg_ns={host_irq_dispatch['avg']}\n")
            f.write(f"host_irq_dispatch_max_ns={host_irq_dispatch['max']}\n")
        if host_irq_handler:
            f.write(f"host_irq_handler_count={host_irq_handler['count']}\n")
            f.write(f"host_irq_handler_avg_ns={host_irq_handler['avg']}\n")
            f.write(f"host_irq_handler_max_ns={host_irq_handler['max']}\n")
        if linux_runtime:
            f.write(f"linux_runtime_ns={linux_runtime['duration_ns']}\n")
            f.write(f"linux_runtime_iterations={linux_runtime['iterations']}\n")
            f.write(f"linux_stress_procs={linux_runtime['stress_procs']}\n")
            if linux_runtime["duration_ns"] > 0:
                transactions_per_s = len(rows) * 1_000_000_000 / linux_runtime["duration_ns"]
                f.write(f"linux_transactions_per_s={transactions_per_s:.6f}\n")
        f.write(f"linux_cpu_count={len(linux_cpus)}\n")
        for cpu, values in sorted(linux_cpus.items()):
            f.write(f"linux_cpu_{cpu}_busy_ticks={values['busy_ticks']}\n")
            f.write(f"linux_cpu_{cpu}_total_ticks={values['total_ticks']}\n")
            f.write(f"linux_cpu_{cpu}_usage_permille={values['usage_permille']}\n")
        if rtthread_stats:
            for key, value in rtthread_stats.items():
                f.write(f"rtthread_{key}={value}\n")
        for key, value in reliability.items():
            f.write(f"reliability_{key}={value}\n")
        for idx, line in enumerate(rt_trace[-16:], start=1):
            f.write(f"rt_trace_tail_{idx}={line}\n")


def main():
    parser = argparse.ArgumentParser(description="Extract AICP metrics from an AxVisor QEMU log.")
    parser.add_argument("log")
    parser.add_argument("--csv", required=True)
    parser.add_argument("--summary", required=True)
    parser.add_argument("--expected", type=int)
    args = parser.parse_args()

    (
        rows,
        done,
        rt_trace,
        rtos_request_timing,
        rtos_periodic,
        linux_cpus,
        linux_runtime,
        rtthread_stats,
        reliability,
    ) = parse_log(args.log)
    write_csv(args.csv, rows)
    write_summary(
        args.summary,
        rows,
        done,
        rt_trace,
        rtos_request_timing,
        rtos_periodic,
        linux_cpus,
        linux_runtime,
        rtthread_stats,
        reliability,
        args.expected,
    )
    if args.expected is not None and len(rows) != args.expected:
        return 1
    if done and done["failed"] != 0:
        return 1
    return 0 if rows else 1


if __name__ == "__main__":
    raise SystemExit(main())
