# Zephyr AxVisor IVC Publisher

This Zephyr app publishes an AxVisor IVC shared-memory channel and sends fixed
messages to a Linux subscriber guest.

The app uses the current AxVisor HVC-based IVC ABI:

- publish channel `0x49564301`;
- initialize the axivc v2 shared-memory region;
- map the shared channel as write-back cacheable memory with `K_MEM_CACHE_WB`;
- run the Zephyr/Linux shared-memory throughput test.

The shared-memory control protocol uses release/acquire state transitions:
Linux publishes `READY` after writing `read_mem`, Zephyr observes `READY`,
copies `read_mem` to `write_mem`, and publishes `DONE` after the copy. This
assumes a cache-coherent platform; non-coherent targets need cache maintenance
hooks at the same publish/observe points.

Build example:

```sh
ZEPHYR_BASE=/path/to/zephyr \
apps/zephyr/ivc_publisher/build.sh
```

The default output is:

```text
tmp/axbuild/zephyr/ivc_publisher/zephyr-ivc-publisher.bin
```
