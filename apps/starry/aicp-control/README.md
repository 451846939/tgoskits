# StarryOS AICP Control App

This StarryOS app replaces the Linux guest in the AI + RTOS control demo.  The
same static AICP client used by the Linux initramfs is compiled with StarryOS
labels and injected into the Starry rootfs as `/usr/bin/aicp_starry_init`.
Rootfs injection requires `debugfs` from e2fsprogs; set `DEBUGFS=/path/to/debugfs`
if it is not available on `PATH`.

The StarryOS kernel path includes the network ioctls needed by this init-style
client: `SIOCSIFADDR`, `SIOCSIFNETMASK`, and `SIOCSIFFLAGS` update the ax-net
runtime interface state, while `SIOCADDRT` and `SIOCSARP` are accepted as
compatibility management operations for the existing connected route and ARP
path.

Standalone smoke mode uses QEMU user networking and connects to the host at
`10.0.2.2:8800`.  AxVisor mode overrides the same compile-time knobs so the
StarryOS guest uses `10.0.3.3/24` and connects to the RTOS guest at
`10.0.3.2:8800` over the isolated virtio-mmio hub.

Useful commands:

```sh
scripts/ai-rtos/run_starry_aicp_smoke.sh 20 ai
scripts/ai-rtos/run_axvisor_starry_rtos_aicp.sh 40 ai 180
```
