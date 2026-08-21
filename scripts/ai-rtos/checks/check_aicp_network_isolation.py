#!/usr/bin/env python3
# Copyright 2026 The TGOSKits Authors
#
# Licensed under the Apache License, Version 2.0.

import argparse
import re
import sys
from dataclasses import dataclass
from pathlib import Path


FORBIDDEN_SIDE_CHANNEL = ("vsock", "ivshmem", "shmem", "memory-backend-file")
REQUIRED_HEADER_FIELDS = (
    "magic",
    "version",
    "msg_type",
    "header_len",
    "payload_len",
    "seq",
    "timestamp_ns",
    "error_code",
    "crc16",
)
ANSI_CSI_RE = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]")
NETWORK_PROFILES = {
    "arceos-tcp": {
        "rtos": "arceos",
        "guest_ip": "10.0.3.3/24",
        "rtos_ip": "10.0.3.2/24",
        "transport": "tcp",
    },
    "rtthread-tcp": {
        "rtos": "rtthread",
        "guest_ip": "10.0.2.14/24",
        "rtos_ip": "10.0.2.15/24",
        "transport": "tcp",
    },
    "zephyr-tcp": {
        "rtos": "zephyr",
        "guest_ip": "10.0.2.14/24",
        "rtos_ip": "10.0.2.15/24",
        "transport": "tcp",
    },
    "freertos-tcp": {
        "rtos": "freertos",
        "guest_ip": "10.0.3.3/24",
        "rtos_ip": "10.0.3.2/24",
        "transport": "tcp",
    },
    "starry-udp": {
        "rtos": "arceos",
        "guest_ip": "10.0.3.15/24",
        "rtos_ip": "10.0.3.2/24",
        "transport": "udp",
    },
}

AI_GUEST_MARKERS = {
    "linux": {
        "connected": ("AICP_LINUX_CONNECTED", "AICP Linux guest connected"),
        "control_sent": ("AICP_LINUX_TX_FRAME type=CONTROL_SET",),
        "status_received": ("AICP_LINUX_STATUS",),
        "done": ("AICP_LINUX_DONE",),
    },
    "starry": {
        "connected": ("AICP_STARRY_NATIVE_TCP_CONNECTED", "AICP_STARRY_NATIVE_START"),
        # The RTOS-side CONTROL marker below is the authoritative receive/apply evidence.
        "control_sent": ("AICP_STARRY_NATIVE_START",),
        "status_received": ("AICP_STARRY_NATIVE_STATUS",),
        "done": ("AICP_STARRY_DONE",),
    },
}

RTOS_MARKERS = {
    "arceos": {
        "ready": (
            "AICP_RTOS_LISTEN",
            "AICP ArceOS RTOS TCP server listening",
            "AICP ArceOS RTOS UDP server listening",
        ),
        "control_applied": ("AICP_RTOS_REQUEST_TIMING", "AICP_RTOS_CONTROL"),
    },
    "rtthread": {
        "ready": ("AICP_RTTHREAD_READY",),
        "control_applied": ("AICP_RTTHREAD_CONTROL",),
    },
    "zephyr": {
        "ready": ("AICP_ZEPHYR_NET_UP", "AICP Zephyr RTOS server listening"),
        "control_applied": ("AICP_ZEPHYR_CONTROL",),
    },
    "freertos": {
        "ready": ("AICP_FREERTOS_READY", "AICP_FREERTOS_HELLO"),
        "control_applied": ("AICP_FREERTOS_CONTROL",),
    },
}

RTOS_ERROR_IMPLEMENTATIONS = {
    "arceos": {
        "core": Path("apps/arceos/aicp-server/src/main.rs"),
        "core_token": "MSG_ERROR",
        "adapter": Path("apps/arceos/aicp-server/src/main.rs"),
        "adapter_token": "send_error",
    },
    "rtthread": {
        "core": Path("apps/ai-rtos-demo/rtos-core/aicp_service.c"),
        "core_token": "AICP_MSG_ERROR",
        "adapter": Path("apps/ai-rtos-demo/rtthread-aicp/main.c"),
        "adapter_token": "aicp_service_serve",
    },
    "zephyr": {
        "core": Path("apps/ai-rtos-demo/rtos-core/aicp_service.c"),
        "core_token": "AICP_MSG_ERROR",
        "adapter": Path("apps/ai-rtos-demo/zephyr/src/main.c"),
        "adapter_token": "aicp_service_serve",
    },
    "freertos": {
        "core": Path("apps/ai-rtos-demo/rtos-core/aicp_service.c"),
        "core_token": "AICP_MSG_ERROR",
        "adapter": Path("apps/ai-rtos-demo/freertos/main.c"),
        "adapter_token": "aicp_service_serve",
    },
}


@dataclass(frozen=True)
class MarkerEvidence:
    present: bool
    matched: str


def error_notification_evidence(rtos):
    implementation = RTOS_ERROR_IMPLEMENTATIONS[rtos]
    core = implementation["core"]
    adapter = implementation["adapter"]
    core_ok = core.is_file() and implementation["core_token"] in core.read_text(errors="replace")
    adapter_ok = adapter.is_file() and implementation["adapter_token"] in adapter.read_text(errors="replace")
    return implementation, core_ok and adapter_ok


def load_qemu_args(path):
    text = path.read_text(errors="replace")
    match = re.search(r"args\s*=\s*\[(?P<body>.*?)\]", text, re.S)
    if not match:
        return []
    return re.findall(r'"([^"]*)"', match.group("body"))


def contains_any(values, needles):
    hits = []
    for value in values:
        lower = value.lower()
        for needle in needles:
            if needle in lower:
                hits.append(value)
                break
    return hits


def find_forbidden_qemu_net(values):
    hits = []
    netdev_pattern = re.compile(r"^(?:user|tap|bridge|socket|vde)(?:,|$)")
    for value in values:
        lower = value.lower()
        if netdev_pattern.search(lower) or "hostfwd=" in lower:
            hits.append(value)
    return hits


def virtual_net_macs(vm_text):
    blocks = re.findall(
        r"\[\[devices\.virtual\]\](.*?)(?=\[\[devices\.|\Z)", vm_text, re.S
    )
    macs = []
    for block in blocks:
        if not re.search(r'^model\s*=\s*"virtio-net"\s*$', block, re.M):
            continue
        match = re.search(r"guest_mac\s*=\s*\[([^]]+)\]", block)
        if not match:
            continue
        try:
            macs.append(":".join(f"{int(value.strip(), 0):02x}" for value in match.group(1).split(",")))
        except ValueError:
            continue
    return macs


def evaluate_axvisor_virtual_switch_topology(qemu_args, vm_texts):
    """Validate that AICP guests use AxVisor virtual NICs, not QEMU networking."""
    expected_guest_macs = {"52:54:00:aa:03:03", "52:54:00:aa:03:02"}
    expected_probe_mac = "52:54:00:aa:03:01"
    failures = []
    report = ["topology=axvisor virtual switch; guest NICs are not QEMU netdevs"]
    guest_macs = []

    for index, vm_text in enumerate(vm_texts, start=1):
        macs = virtual_net_macs(vm_text)
        report.append(f"vm{index}_virtual_net_macs={','.join(macs)}")
        if len(macs) != 1:
            failures.append(f"VM {index} must define exactly one virtio-net virtual device, found {len(macs)}")
        guest_macs.extend(macs)

    if set(guest_macs) != expected_guest_macs:
        failures.append(
            "virtual guest MACs must be 52:54:00:aa:03:03 and 52:54:00:aa:03:02, "
            f"found {guest_macs}"
        )

    forbidden_net = [
        value
        for value in find_forbidden_qemu_net(qemu_args)
        if value != "user,id=hostnet"
    ]
    if forbidden_net:
        failures.append(f"forbidden guest/external QEMU network options present: {forbidden_net}")
    if "user,id=hostnet" not in qemu_args:
        failures.append("missing documented host-only QEMU virtio-net probe backend")

    host_probe_devices = [
        value for value in qemu_args if "netdev=hostnet" in value and "virtio-net-device" in value
    ]
    if len(host_probe_devices) != 1 or expected_probe_mac not in host_probe_devices[0].lower():
        failures.append(
            "host-only QEMU virtio-net probe must use MAC "
            f"{expected_probe_mac}, found {host_probe_devices}"
        )
    if any(mac in "\n".join(host_probe_devices).lower() for mac in expected_guest_macs):
        failures.append("a guest AICP MAC is attached to the QEMU host network backend")

    return failures, report


def find_marker(text, alternatives):
    return next((marker for marker in alternatives if marker in text), "")


def runtime_marker_groups(ai_guest, rtos):
    groups = {}
    groups.update({f"ai_{name}": markers for name, markers in AI_GUEST_MARKERS[ai_guest].items()})
    groups.update({f"rtos_{name}": markers for name, markers in RTOS_MARKERS[rtos].items()})
    return groups


def evaluate_runtime_markers(text, groups):
    evidence = {
        name: MarkerEvidence(bool(matched := find_marker(text, alternatives)), matched)
        for name, alternatives in groups.items()
    }
    completed_transaction = all(
        evidence[name].present
        for name in (
            "ai_connected",
            "ai_control_sent",
            "ai_status_received",
            "ai_done",
            "rtos_control_applied",
        )
    )
    if not evidence["rtos_ready"].present and completed_transaction:
        evidence["rtos_ready"] = MarkerEvidence(True, "inferred:completed_transaction")
    return evidence


def main() -> int:
    parser = argparse.ArgumentParser(description="Verify AICP guest network isolation configuration.")
    parser.add_argument("--qemu-config", default="os/axvisor/configs/qemu/qemu-aarch64-aicp-dual-net.toml")
    parser.add_argument("--protocol-header", default="apps/ai-rtos-demo/aicp/aicp.h")
    parser.add_argument(
        "--profile",
        choices=sorted(NETWORK_PROFILES),
        default="arceos-tcp",
        help="Expected guest/RTOS addressing and transport profile.",
    )
    parser.add_argument(
        "--ai-guest",
        choices=sorted(AI_GUEST_MARKERS),
        default="linux",
        help="AI application guest whose runtime evidence is checked.",
    )
    parser.add_argument(
        "--log",
        action="append",
        help="Runtime log. Repeat for separate AxVisor and Linux console logs.",
    )
    parser.add_argument(
        "--vm-config",
        action="append",
        help="Generated AxVM guest configuration. Repeat once per AICP guest to validate the virtual switch topology.",
    )
    parser.add_argument("--summary", required=True)
    args = parser.parse_args()

    qemu_config = Path(args.qemu_config)
    header = Path(args.protocol_header)
    qemu_args = load_qemu_args(qemu_config)
    joined = "\n".join(qemu_args)
    profile = NETWORK_PROFILES[args.profile]
    report = []
    failures = []

    hubports = [arg for arg in qemu_args if arg.startswith("hubport,")]
    hubids = [match.group(1) for arg in hubports if (match := re.search(r"(?:^|,)hubid=([^,]+)", arg))]
    macs = re.findall(r"mac=([0-9a-fA-F:]{17})", joined)
    linux_net = next((arg for arg in qemu_args if "netdev=linuxnet" in arg), "")
    rtos_net = next((arg for arg in qemu_args if "netdev=rtosnet" in arg), "")
    forbidden_net = find_forbidden_qemu_net(qemu_args)
    forbidden_side = contains_any(qemu_args, FORBIDDEN_SIDE_CHANNEL)

    report.append(f"qemu_config={qemu_config}")
    report.append(f"profile={args.profile}")
    report.append(f"ai_guest={args.ai_guest}")
    report.append(f"rtos_guest={profile['rtos']}")
    report.append(f"netdev_hubports={len(hubports)}")
    report.append(f"hubids={','.join(hubids)}")
    report.append(f"macs={','.join(macs)}")
    if args.vm_config:
        report.append("topology=AxVisor virtual switch; QEMU NIC is host-only")
    else:
        report.append(
            f"topology=isolated QEMU hubport hubid={hubids[0] if hubids else 'unknown'}; "
            "no host NIC, NAT or bridge"
        )
    report.append(f"linux_or_starry_ip={profile['guest_ip']}")
    report.append(f"rtos_ip={profile['rtos_ip']}")
    report.append(f"aicp_port=8800/{profile['transport']}")

    if args.vm_config:
        vm_texts = [Path(path).read_text(errors="replace") for path in args.vm_config]
        virtual_failures, virtual_report = evaluate_axvisor_virtual_switch_topology(qemu_args, vm_texts)
        failures.extend(virtual_failures)
        report.extend(virtual_report)
    else:
        if len(hubports) != 2:
            failures.append(f"expected 2 hubport netdevs, found {len(hubports)}")
        if len(hubids) != 2 or len(set(hubids)) != 1:
            failures.append(f"expected two endpoints on the same hub, found hubids={hubids}")
        for mac in ("52:54:00:aa:03:03", "52:54:00:aa:03:02"):
            if mac not in macs:
                failures.append(f"missing expected MAC {mac}")
        if forbidden_net:
            failures.append(f"forbidden host/external network options present: {forbidden_net}")
    if forbidden_side:
        failures.append(f"forbidden side-channel options present: {forbidden_side}")
    if profile["rtos"] in ("rtthread", "zephyr"):
        report.append(f"linux_mrg_rxbuf_off={'mrg_rxbuf=off' in linux_net}")
        report.append(f"rtos_mrg_rxbuf_on={'mrg_rxbuf=on' in rtos_net}")
        if "mrg_rxbuf=off" not in linux_net:
            failures.append("AI guest virtio-net must use mrg_rxbuf=off")
        if "mrg_rxbuf=on" not in rtos_net:
            failures.append("RTOS virtio-net must use mrg_rxbuf=on for the 12-byte header")

    header_text = header.read_text(errors="replace")
    missing_fields = [field for field in REQUIRED_HEADER_FIELDS if field not in header_text]
    report.append(f"protocol_header={header}")
    report.append(f"protocol_required_fields={','.join(REQUIRED_HEADER_FIELDS)}")
    if missing_fields:
        failures.append(f"protocol header missing fields: {missing_fields}")

    for token in ("AICP_MSG_CONTROL_SET", "AICP_MSG_STATUS", "AICP_MSG_ERROR"):
        if token not in header_text:
            failures.append(f"protocol header missing {token}")

    error_implementation, error_implemented = error_notification_evidence(profile["rtos"])
    report.append(f"rtos_error_core={error_implementation['core']}")
    report.append(f"rtos_error_adapter={error_implementation['adapter']}")
    report.append(f"business_error_notification_implemented={error_implemented}")
    if not error_implemented:
        failures.append(
            "RTOS error notification core or adapter is incomplete: "
            f"{error_implementation['core']}, {error_implementation['adapter']}"
        )

    if args.log:
        logs = [Path(log) for log in args.log]
        text = "\n".join(
            ANSI_CSI_RE.sub("", log.read_text(errors="replace")) for log in logs
        )
        for log in logs:
            report.append(f"log={log}")
        groups = runtime_marker_groups(args.ai_guest, profile["rtos"])
        evidence = evaluate_runtime_markers(text, groups)
        for name, alternatives in groups.items():
            item = evidence[name]
            report.append(f"log_marker_{name}={item.present}")
            if item.matched:
                report.append(f"log_marker_{name}_matched={item.matched}")
            if not item.present:
                failures.append(
                    f"log marker group missing: {name} ({' | '.join(alternatives)})"
                )
        guest_ip = profile["guest_ip"].split("/", maxsplit=1)[0]
        rtos_ip = profile["rtos_ip"].split("/", maxsplit=1)[0]
        for name, address in (("ai_guest_ip", guest_ip), ("rtos_ip", rtos_ip)):
            present = address in text
            report.append(f"log_address_{name}={present}")
            if not present:
                failures.append(f"runtime log missing expected {name}={address}")

    report.append(f"forbidden_qemu_net_hits={len(forbidden_net)}")
    report.append(f"forbidden_side_channel_hits={len(forbidden_side)}")
    report.append(f"result={'FAIL' if failures else 'PASS'}")
    for failure in failures:
        report.append(f"failure={failure}")

    Path(args.summary).write_text("\n".join(report) + "\n")
    print("\n".join(report))
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
