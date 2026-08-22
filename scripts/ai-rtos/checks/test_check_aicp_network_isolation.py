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


class AxVisorVirtualSwitchTopologyTest(unittest.TestCase):
    def test_aicp_qemu_profile_avoids_an_unused_host_root_disk(self):
        qemu_profile = Path("os/axvisor/configs/qemu/qemu-aarch64-aicp-dual-net.toml")
        board_profile = Path("os/axvisor/configs/board/qemu-aarch64-aicp-dual.toml")
        runner = Path("scripts/ai-rtos/runners/run_axvisor_dual_guest_aicp.sh")

        self.assertNotIn("nvme,drive=disk0", qemu_profile.read_text())
        self.assertNotIn("root=/dev/nvme0n1", qemu_profile.read_text())
        self.assertIn("features = []", board_profile.read_text())
        self.assertIn(str(board_profile), runner.read_text())

    def test_virtual_guest_network_accepts_only_the_documented_host_probe(self):
        qemu_args = [
            "user,id=hostnet",
            "virtio-net-device,netdev=hostnet,mac=52:54:00:aa:03:01",
        ]
        vm_texts = [
            """
[[devices.virtual]]
id = "aicp-net"
model = "virtio-net"
guest_mac = [0x52, 0x54, 0x00, 0xaa, 0x03, 0x03]
""",
            """
[[devices.virtual]]
id = "aicp-net"
model = "virtio-net"
guest_mac = [0x52, 0x54, 0x00, 0xaa, 0x03, 0x02]
""",
        ]

        failures, report = MODULE.evaluate_axvisor_virtual_switch_topology(qemu_args, vm_texts)

        self.assertEqual(failures, [])
        self.assertTrue(
            any(item.startswith("topology=axvisor virtual switch") for item in report)
        )

    def test_virtual_guest_network_rejects_a_guest_bypass_or_missing_mac(self):
        qemu_args = [
            "user,id=hostnet,hostfwd=tcp::8800-:8800",
            "virtio-net-device,netdev=hostnet,mac=52:54:00:aa:03:03",
        ]
        vm_texts = [
            """
[[devices.virtual]]
id = "aicp-net"
model = "virtio-net"
guest_mac = [0x52, 0x54, 0x00, 0xaa, 0x03, 0x03]
""",
            """
[[devices.virtual]]
id = "aicp-net"
model = "virtio-blk"
""",
        ]

        failures, _ = MODULE.evaluate_axvisor_virtual_switch_topology(qemu_args, vm_texts)

        self.assertTrue(any("hostfwd" in failure for failure in failures))
        self.assertTrue(any("virtio-net" in failure for failure in failures))
        self.assertTrue(any("52:54:00:aa:03:02" in failure for failure in failures))


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
