# Axvisor Shared Firmware Provider Mediation

This document defines how an AArch64 passthrough guest may retain access to a
physical firmware provider that is also required by a host-owned virtualized
device. It is an MMIO ownership and failure contract for machine planning,
driver capabilities, `DeviceRuntime`, and stage-2 construction.

Implementation base: `origin/dev` at
`a6e8f239ea8565888bea1fdf33e192ea130d5815`.

## Problem and Success Criteria

An FDT-backed Axvisor machine replaces the firmware-selected physical UART with
a virtual UART at the same guest address and interrupt identity. The physical
UART and its clock remain host-owned. A passthrough guest nevertheless starts
with an identity mapping of assignable physical address space, including a
shared clock/reset unit unless another resource punches a stage-2 hole.

After the physical UART node is replaced, Linux no longer sees a consumer for
its original clock. `clk_disable_unused` can then write the shared provider and
gate the host UART while the guest continues running. On RK3568 this manifested
as a repeatable loss of host console output during guest boot. Adding
`clk_ignore_unused` allowed boot to continue, which proved the ownership
violation but was not an acceptable fix.

The implementation succeeds when:

- the physical UART remains unavailable for explicit or default assignment;
- the guest still observes and can use unrelated functions in the shared
  provider;
- writes that can gate, reparent, or corrupt the host UART clock are removed;
- the complete provider MMIO range is absent from passthrough stage-2 mappings
  and is served through one `DeviceRuntime` device;
- no board name, SoC compatible string, or register constant enters the AxVM
  runtime access path;
- an unknown or ambiguous mutable provider prevents VM construction instead of
  falling back to unmediated passthrough;
- RK3568 and RK3588 boot without `clk_ignore_unused`.

## Scope and Non-goals

The first consumer is the clock dependency of the host-selected UART on
RK3568 and RK3588. The contracts are provider-neutral so reset or power-domain
drivers can expose equivalent protection capabilities later.

This design does not:

- expose the physical UART to a guest;
- emulate a complete clock tree or allow a guest to configure the host UART;
- accept register rules from guest TOML or another untrusted input;
- infer protection from board names or generic clock IDs inside AxVM;
- support mutable providers with multiple MMIO ranges or non-single-cell clock
  selectors until a typed provider capability describes them;
- add big-endian guest MMIO semantics. The current AArch64 Axvisor target and
  Rockchip providers are little-endian.

Fixed clocks and other providers without mutable MMIO need no mediator.

## Alternatives

`clk_ignore_unused` is a guest-kernel workaround and leaves other clock writes
able to disable the host UART. Removing the entire provider from the guest
would protect the host but break unrelated passthrough devices. A full shadow
clock controller would duplicate a large hardware-specific state machine and
would still need a policy for host-owned leaves. Filtering writes in AxVM with
RK3568/RK3588 constants would solve the observed symptom but create a platform
policy layer in the VM hot path.

The selected design keeps hardware register knowledge in the Rockchip clock
driver, translates it through a typed `rdif-clk` capability, and uses the
existing transactional device runtime for resource ownership and dispatch.

## Immutable Machine Identity

The serial FDT parser retains every `clocks` reference as:

- provider phandle;
- complete clock specifier cells;
- provider `reg` regions.

This data is validated while firmware is available and becomes part of
`GuestSerialFdtIdentity`. The guest-visible UART node receives a virtual fixed
clock, but the original references remain internal machine-plan evidence. They
are not configuration fields and cannot be overridden by TOML.

An absent provider `reg` means there is no mutable MMIO to mediate. A mutable
provider currently requires exactly one region and one selector cell. Missing
phandles, malformed `#clock-cells`, truncated specifiers, invalid regions, or
ambiguous shapes are errors.

## Typed Provider Capability

`rdif-clk` exposes immutable assignment protection rules for a requested clock
ID:

- `None` means the provider cannot safely mediate that assignment;
- an empty list means no mutable MMIO state needs protection;
- a non-empty list defines all provider-owned protected writes.

The initial rule forms are:

- `Deny { offset, length }`, which suppresses every overlapping write;
- `MaskedWrite32 { offset, value_mask, write_enable_mask }`, which accepts only
  an aligned 32-bit write and removes both the protected value bits and their
  write-enable bits before forwarding the remainder.

Rules are produced by the hardware driver and validated against the provider
region before a VM becomes runnable. They must use provider-relative offsets,
non-empty ranges, aligned masked registers, disjoint nonzero masks, and bounded
arithmetic.

Rockchip high-half write-mask registers allow unrelated fields to be forwarded
without a software read-modify-write lock. Fractional divider registers are
denied as a unit because numerator and denominator form one indivisible clock
configuration.

## Factory, Resource, and Access Flow

Provider resolution occurs in task context during architecture resource
construction:

1. the AArch64 planner ignores references for a `Virtualized` guest;
2. for a `Passthrough` guest it resolves the provider phandle through `rdrive`;
3. it requests protection rules through the typed clock capability;
4. references to the same provider are merged and deduplicated;
5. one immutable internal `EmulatedDeviceConfig` is generated per provider;
6. the architecture registers one factory containing the validated plans;
7. the factory maps the physical provider and returns a `SharedMmioDevice`;
8. the device claims the complete provider MMIO range;
9. the address planner treats that resource as an emulated-device hole, so no
   stage-2 passthrough mapping overlaps it.

The internal device config contains a plan index and provider phandle as a
fingerprint. The factory compares the full name, type, range, index, and
phandle before mapping hardware. It does not interpret guest configuration.
Normal `DeviceBundle` registration provides atomic resource rollback if later
construction fails.

Reads are forwarded at their original width. Writes are range- and
alignment-checked, filtered using the immutable rules, and either forwarded or
suppressed. The runtime path performs no driver lookup, allocation, VM lookup,
callback, or provider lock acquisition.

## Concurrency and Lifecycle

Provider rules and the mapped region are immutable after factory construction.
The MMIO device can be called concurrently by vCPUs. Rockchip masked writes
remain atomic hardware operations, and denied registers never reach hardware.
No shadow clock state or second pending queue exists.

The host clock driver may still access the same physical provider through its
own typed operations. Safe sharing therefore depends on hardware operations
that do not require a software read-modify-write transaction. A future provider
that requires serialization must expose that serialization as part of its
runtime capability rather than adding a global AxVM lock.

Stopping or destroying a VM drops the device runtime and its mapping. The
physical provider remains owned by the host driver; guest lifecycle operations
never reset or disable it.

## Failure Policy

The planner rejects:

- a mutable provider with no registered typed capability;
- an unsupported provider region/specifier layout;
- invalid or out-of-range protection rules;
- inconsistent regions for one phandle;
- a factory config that does not match its validated plan;
- failure to map the physical provider.

There is no raw-passthrough fallback for these cases. A safe fixed provider is
represented by the absence of mutable provider regions, not by ignoring an
error.

## Validation

The deterministic regression is:

```text
cargo test -p axvm --no-default-features --features host-test \
  shared_mmio::tests::strips_rk3568_uart2_gate_disable_write -- --exact
```

Before the filter implementation, the real RK3568
`0x0009_0009` gate-disable write was forwarded. The same test now suppresses
it. Additional unit coverage checks unrelated bit forwarding, partial/denied
writes, zero writes to unprotected registers, resource identity, and MMIO
read/write forwarding. Rockchip tests pin the complete UART2 gate, selector,
mux, and fractional-divider rules for both RK3568 and RK3588. FDT tests cover
multi-clock parsing and malformed specifiers. Existing address-layout tests
prove that every emulated MMIO resource punches a passthrough hole.

Integration validation must include:

- an AArch64 Axvisor target build;
- QEMU GICv2 and GICv3 timer stress to protect the adjacent timer work;
- three consecutive RK3568 boots reaching the configured guest marker without
  a host-time jump or console loss;
- RK3588/OrangePi-5-Plus boot without `clk_ignore_unused`;
- formatter and targeted Clippy checks for `rdif-clk`, `rockchip-soc`,
  `ax-driver`, `axvm-types`, and `axvm`.
