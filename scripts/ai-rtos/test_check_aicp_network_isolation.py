#!/usr/bin/env python3
# Copyright 2026 The TGOSKits Authors
#
# Licensed under the Apache License, Version 2.0.

import importlib.util
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("check_aicp_network_isolation.py")
SPEC = importlib.util.spec_from_file_location("check_aicp_network_isolation", SCRIPT)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class ForbiddenQemuNetworkTest(unittest.TestCase):
    def test_absolute_users_path_is_not_user_network(self):
        values = [
            "/example/work/tgoskits/tmp/linux-qemu",
            "virtio-net-device,netdev=usernet,mac=52:54:00:aa:03:03",
            "hubport,id=usernet,hubid=3",
        ]

        self.assertEqual(MODULE.find_forbidden_qemu_net(values), [])

    def test_external_network_backends_are_rejected(self):
        values = [
            "user,id=net0",
            "tap,id=net1,ifname=tap0",
            "bridge,id=net2,br=br0",
            "socket,id=net3,listen=:1234",
            "vde,id=net4,sock=/tmp/vde.sock",
            "user,id=net5,hostfwd=tcp::8800-:8800",
        ]

        self.assertEqual(MODULE.find_forbidden_qemu_net(values), values)


class InternalAxvisorNetworkTest(unittest.TestCase):
    def test_guest_mac_array_is_normalized(self):
        config = """
[[devices.virtual]]
id = "virtnet0"
model = "virtio-net"
guest_mac = [0x52, 0x54, 0x00, 0xaa, 0x03, 0x02]
"""

        self.assertEqual(
            MODULE.extract_internal_virtio_net_macs(config),
            ["52:54:00:aa:03:02"],
        )

    def test_invalid_guest_mac_is_ignored(self):
        config = 'guest_mac = [0x52, 0x54, 0x00, 0xaa, 0x03, 0x100]'

        self.assertEqual(MODULE.extract_internal_virtio_net_macs(config), [])


class RuntimeMatrixTest(unittest.TestCase):
    def test_common_aicp_stream_is_platform_neutral(self):
        stream_header = Path("apps/ai-rtos-demo/aicp/aicp_stream.h")
        self.assertTrue(stream_header.is_file())
        source = stream_header.read_text(errors="replace")
        for platform_token in ("__ZEPHYR__", "__RTTHREAD__", "AICP_FREERTOS"):
            with self.subTest(platform_token=platform_token):
                self.assertNotIn(platform_token, source)

    def test_all_linux_and_starry_rtos_combinations_have_marker_groups(self):
        for ai_guest in ("linux", "starry"):
            for rtos in ("rtthread", "zephyr", "freertos"):
                with self.subTest(ai_guest=ai_guest, rtos=rtos):
                    groups = MODULE.runtime_marker_groups(ai_guest, rtos)
                    self.assertEqual(
                        set(groups),
                        {
                            "ai_connected",
                            "ai_control_sent",
                            "ai_status_received",
                            "ai_done",
                            "rtos_ready",
                            "rtos_control_applied",
                        },
                    )
                    self.assertTrue(all(groups.values()))

    def test_profiles_cover_three_native_rtos_guests(self):
        expected = {
            "rtthread-tcp": "rtthread",
            "zephyr-tcp": "zephyr",
            "freertos-tcp": "freertos",
        }

        for profile, rtos in expected.items():
            with self.subTest(profile=profile):
                self.assertEqual(MODULE.NETWORK_PROFILES[profile]["rtos"], rtos)
                self.assertEqual(MODULE.NETWORK_PROFILES[profile]["transport"], "tcp")

    def test_zephyr_control_marker_matches_current_adapter_log(self):
        marker = "AICP_ZEPHYR_CONTROL seq=2 target_milli=480"

        self.assertEqual(
            MODULE.find_marker(
                marker,
                MODULE.RTOS_MARKERS["zephyr"]["control_applied"],
            ),
            "AICP_ZEPHYR_CONTROL",
        )

    def test_freertos_hello_is_valid_server_readiness_evidence(self):
        marker = "AICP_FREERTOS_HELLO seq=1"

        self.assertEqual(
            MODULE.find_marker(
                marker,
                MODULE.RTOS_MARKERS["freertos"]["ready"],
            ),
            "AICP_FREERTOS_HELLO",
        )

    def test_completed_transaction_proves_readiness_when_early_log_is_interleaved(self):
        groups = {
            "ai_connected": ("AI_CONNECTED",),
            "ai_control_sent": ("AI_CONTROL",),
            "ai_status_received": ("AI_STATUS",),
            "ai_done": ("AI_DONE",),
            "rtos_ready": ("RTOS_READY",),
            "rtos_control_applied": ("RTOS_CONTROL",),
        }
        text = "\n".join(
            (
                "AI_CONNECTED",
                "AI_CONTROL",
                "RTOS_CONTROL",
                "AI_STATUS",
                "AI_DONE",
            )
        )

        evidence = MODULE.evaluate_runtime_markers(text, groups)

        self.assertTrue(evidence["rtos_ready"].present)
        self.assertEqual(evidence["rtos_ready"].matched, "inferred:completed_transaction")
        self.assertTrue(all(item.present for item in evidence.values()))

    def test_each_rtos_server_implements_error_notification(self):
        for rtos, implementation in MODULE.RTOS_ERROR_IMPLEMENTATIONS.items():
            with self.subTest(rtos=rtos):
                evidence, implemented = MODULE.error_notification_evidence(rtos)
                self.assertEqual(evidence, implementation)
                self.assertTrue(evidence["core"].is_file())
                self.assertTrue(evidence["adapter"].is_file())
                self.assertTrue(implemented)


if __name__ == "__main__":
    unittest.main()
