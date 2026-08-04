# Zephyr AxVisor IVC Publisher

This Zephyr app publishes an AxVisor IVC shared-memory channel and sends fixed
messages to a Linux subscriber guest.

The app uses the current AxVisor HVC-based IVC ABI:

- publish channel `0x49564301`;
- initialize the axivc v2 shared-memory region;
- send five `Request` messages on the publisher-to-subscriber ring;
- notify Linux VM `2` after each send;
- receive Linux `Ack` messages on the subscriber-to-publisher ring.

Build example:

```sh
ZEPHYR_BASE=/path/to/zephyr \
apps/zephyr/ivc_publisher/build.sh
```

The default output is:

```text
tmp/axbuild/zephyr/ivc_publisher/zephyr-ivc-publisher.bin
```
