# AI/RTOS Network Topology

The primary data path is an IP network link. Shared memory, hypercalls, raw MMIO
and vsock are not used for AICP control/status traffic.

| Node | Interface | MAC policy | IP | Port | Notes |
| --- | --- | --- | --- | --- | --- |
| Linux/Starry AI guest | `eth0` | static or virtio generated | `192.168.70.10/24` | client | Sends AICP `CONTROL_SET`, receives `STATUS`/`ERROR`. |
| RTOS control guest | `eth0` | static or virtio generated | `192.168.70.20/24` | `8800/tcp` | Runs AICP server and control loop. |
| Host/QEMU bridge | `br-aicp` | host managed | `192.168.70.1/24` | none | Test bridge, not routed to the public network by default. |

Recommended host bridge sketch for QEMU validation:

```sh
sudo ip link add br-aicp type bridge
sudo ip addr add 192.168.70.1/24 dev br-aicp
sudo ip link set br-aicp up
```

For access control, only TCP port `8800` on the RTOS guest is required for the
demo. Keep the bridge isolated unless board-level validation explicitly needs an
external uplink.

For the checked-in host smoke test, the same AICP protocol is exercised on
loopback (`127.0.0.1:${AICP_PORT:-18800}`) so protocol framing, CRC, reconnect
and latency collection can be validated before RTOS image integration.

AxVisor integration note: this repository currently defines the `VirtioNet`
emulated device type, but no built-in virtio-net factory is registered by
`axdevice`. For pure QEMU dual-guest IP networking, add a virtio-net/tap backend
and connect both guest NICs to `br-aicp`; for board validation, use a physical
NIC passthrough or board-supported virtual network path and keep this addressing
plan unchanged.
