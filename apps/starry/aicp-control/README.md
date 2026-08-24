# StarryOS AICP Control App

This StarryOS app replaces the Linux guest in the AI + RTOS control demo.  The
same static AICP client used by the Linux initramfs is compiled with StarryOS
labels and injected into the Starry rootfs as `/usr/bin/aicp_starry_init`.
Rootfs injection requires `debugfs` from e2fsprogs; set `DEBUGFS=/path/to/debugfs`
if it is not available on `PATH`.

The standalone QEMU profile uses the address acquired by StarryOS DHCP.  Its
AICP client therefore leaves the runtime-owned interface configuration intact
and only opens the TCP/IP control connection after DHCP is ready.  Other
profiles can retain the static-interface setup path when their guest image
does not provide DHCP.

Standalone smoke mode uses QEMU user networking and connects to the host at
`10.0.2.2:8800`.  AxVisor mode overrides the same compile-time knobs so the
StarryOS guest uses `10.0.3.3/24` and connects to the RTOS guest at
`10.0.3.2:8800` over the isolated virtio-mmio hub.

The standalone smoke runner is an implementation-level diagnostic, rather than
the supported multi-guest reproduction entry point. The current latest-dev
validated AICP closed loop is Linux + ArceOS; use the single public entry point:

```sh
scripts/ai-rtos/aicp.sh doctor
scripts/ai-rtos/aicp.sh smoke
```
