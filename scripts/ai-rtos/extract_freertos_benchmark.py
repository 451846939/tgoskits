#!/usr/bin/env python3
# Copyright 2026 The TGOSKits Authors
#
# SPDX-License-Identifier: Apache-2.0

"""Extract FreeRTOS benchmark tables from an AxVisor/QEMU serial log."""

from __future__ import annotations

import argparse
import re
from pathlib import Path


SECTION_RE = re.compile(r"^\*\*\s+(?P<section>.+?)\s+\[avg,\s*min,\s*max\].*\*\*")
ROW_RE = re.compile(
    r"^\s*(?P<name>[^:]+?)\s*:\s+"
    r"(?P<avg>n/a|-?\d+),\s+"
    r"(?P<min>n/a|-?\d+),\s+"
    r"(?P<max>n/a|-?\d+)\s*$"
)


def clean_text(text: str) -> str:
    # QEMU logs can contain ANSI color and CRLF serial output.
    text = re.sub(r"\x1b\[[0-9;]*m", "", text)
    return text.replace("\r", "")


def metric_key(section: str, row: str) -> str:
    raw = f"{section}_{row}".lower()
    raw = re.sub(r"\([^)]*\)", "", raw)
    raw = re.sub(r"[^a-z0-9]+", "_", raw).strip("_")
    return raw


def parse_value(value: str) -> int | None:
    if value == "n/a":
        return None
    return int(value)


def extract(path: Path) -> dict[str, int | str | None]:
    metrics: dict[str, int | str | None] = {
        "source_log": str(path),
        "marker_started": 0,
        "marker_done": 0,
    }
    current_section: str | None = None
    for line in clean_text(path.read_text(errors="replace")).splitlines():
        if "freeRTOS benchmark started" in line:
            metrics["marker_started"] = 1
        if "freeRTOS benchmark done" in line:
            metrics["marker_done"] = 1

        section_match = SECTION_RE.search(line)
        if section_match:
            current_section = section_match.group("section")
            continue

        if current_section is None:
            continue
        row_match = ROW_RE.search(line)
        if not row_match:
            continue

        key = metric_key(current_section, row_match.group("name"))
        metrics[f"{key}_avg_ns"] = parse_value(row_match.group("avg"))
        metrics[f"{key}_min_ns"] = parse_value(row_match.group("min"))
        metrics[f"{key}_max_ns"] = parse_value(row_match.group("max"))
    return metrics


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("log", type=Path)
    parser.add_argument("--summary", type=Path, required=True)
    args = parser.parse_args()

    metrics = extract(args.log)
    args.summary.parent.mkdir(parents=True, exist_ok=True)
    with args.summary.open("w", encoding="utf-8") as out:
        for key in sorted(metrics):
            value = metrics[key]
            out.write(f"{key}={'' if value is None else value}\n")
    return 0 if metrics.get("marker_done") == 1 else 1


if __name__ == "__main__":
    raise SystemExit(main())
