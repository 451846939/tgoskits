# Axvisor IVC Linux Guest Support

This directory contains the Linux-side user-space pieces used by the Axvisor
IVC QEMU test:

- `include/`: shared ioctl and user library headers.
- `lib/`: small userspace wrapper over the IVC device ioctls.
- `publisher/`: Linux publisher program for Linux-to-ArceOS tests.
- `subscriber/`: Linux subscriber program used by the ArceOS-to-Linux test.

The Linux kernel module that exposes `/dev/axivc` is not kept in tgoskits. It
is built by tgosimages together with the target Linux kernel and installed into
the rootfs as `/root/axvisor.ko`.

Build the test payloads with:

```bash
AXVISOR_IVC_ARCH=aarch64 \
AXVISOR_IVC_OUT_DIR=/path/to/out \
apps/linux/ivc/build.sh
```

The output directory contains:

```text
ivc-publish
ivc-subscribe
```

The performance subscriber defaults to mmap through `/dev/axivc_subscriber_N`.
With newer drivers it queries `IVC_GET_CHANNEL_INFO` from the subscriber device
and uses the returned shared-memory size for mmap. If the driver mmap path is
unavailable, it falls back to `/dev/mem`.

Debug environment variables:

- `AXIVC_MMAP=driver`: require the driver mmap path.
- `AXIVC_MMAP=devmem`: force the `/dev/mem` fallback path.
- `AXIVC_DEVMEM_BASE=<addr>`: override the `/dev/mem` physical base.

The Zephyr/Linux performance protocol currently maps the shared memory as
Normal cacheable memory and requires cross-guest cache coherency. Linux publishes
`READY` with release ordering after writing `read_mem`; Zephyr observes it with
acquire ordering, copies to `write_mem`, then publishes `DONE`; Linux observes
`DONE` with acquire ordering before reading `write_mem`. Non-cache-coherent
targets need cache maintenance hooks around these same publish/observe points.

`cargo xtask axvisor test qemu --arch aarch64 --test-case ivc` builds these
payloads as part of the test and injects them into the selected Linux rootfs.
