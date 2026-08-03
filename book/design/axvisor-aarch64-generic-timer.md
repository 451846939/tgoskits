# Axvisor AArch64 Generic Timer Virtualization

## Status

This document defines the incompatible AArch64 generic-timer model used by
Axvisor. It is the ownership and world-switch contract for the implementation;
changes to timer state, counter offsets, PPI completion, vCPU migration, or
firmware resources must update this document before merge.

Implementation base: `origin/dev` at `a6e8f239ea8565888bea1fdf33e192ea130d5815`.

The design applies uniformly to QEMU GICv2, QEMU GICv3, RK3568, RK3588, and
other AArch64 hosts. It must not be repaired with board-name, SoC-compatible,
or GIC-version special cases.

## Problem

The previous implementation split one architectural timer across unrelated
owners:

- guest timer registers lived partly in `GuestSystemRegisters`;
- a VM-level relay copied some `CNTV` state;
- a bus-shaped emulated timer device owned another software model;
- the WFI timer wheel independently inferred a wakeup;
- the virtual GIC received a PPI level without owning the complete
  pending/active/EOI lifecycle;
- `CNTVOFF_EL2` was cleared while `CNTV_CTL_EL0` could still be enabled.

The last ordering error temporarily moved a guest CVAL into the host physical
counter epoch. On RK3568, the guest-visible time moved from approximately
3.96 seconds to 78.86 seconds and the boot later timed out. RK3568 and RK3588
both use GICv3, so a GIC-version branch cannot explain or correctly repair this
failure.

The replacement must provide these observable properties:

1. Every vCPU has one canonical virtual and physical timer context.
2. Every vCPU in a VM shares the same immutable counter frequency and offsets.
3. No Rust host code runs while the guest timer or guest counter offset remains
   installed.
4. Timer output is a level input; VGIC exclusively owns pending, active,
   enable, route, EOI, and DIR state.
5. An acknowledged host CNTV PPI is deactivated only after the matching guest
   delivery retires, except for explicit migration or teardown.
6. WFI uses the earliest deliverable timer deadline and never turns a stale
   callback directly into a forced PPI.
7. Host firmware and runtime consume one validated timer profile.

## Reference Model

The implementation follows the KVM nVHE ownership model in Linux commit
`8cd9520d35a6c38db6567e97dd93b1f11f185dc6`:

- [`struct arch_timer_vm_data` and `struct arch_timer_context`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/include/kvm/arm_arch_timer.h)
  separate VM offsets from per-vCPU timer contexts and track whether a context
  is loaded in hardware.
- [`timer_save_state`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/arch/arm64/kvm/arch_timer.c)
  reads CTL/CVAL, disables the timer, executes an ISB, and only then clears the
  counter offset.
- `timer_restore_state` installs the offset and CVAL before enabling CTL.
- `kvm_timer_blocking` schedules the earliest timer capable of waking a
  blocked vCPU.
- `kvm_timer_vcpu_load_gic` reconciles the timer output with the virtual GIC
  active state instead of treating the host PPI as an independent software
  interrupt.

Axvisor does not import Linux hrtimers, nested virtualization, VHE-specific
switches, or userspace irqchip fallbacks. It preserves the same ownership and
ordering with its own timer wheel and unified VGIC controller.

## State Ownership

| State | Owner | Notes |
| --- | --- | --- |
| counter frequency and offsets | `ArmTimerVmConfig` | immutable and identical for all VM vCPUs |
| CNTV CVAL, ENABLE, IMASK | vCPU `ArmTimerContext` | loaded directly into hardware |
| CNTP CVAL, ENABLE, IMASK | vCPU `ArmTimerContext` | accessed through trapped software emulation |
| ISTATUS | derived | computed from current counter and CVAL; never writable or saved |
| timer output level | vCPU snapshot plus `Aarch64TimerBinding` | published into one private VGIC line |
| PPI pending/active/enable/EOI | `arm_vgic` | canonical interrupt state |
| acknowledged host CNTV token | host timer-PPI binding | lock-out resource retained until VGIC retirement |
| assigned-SPI pending/active state | physical GIC plus HW-backed LR | LR carries the ownership-checked physical INTID |
| WFI wake event | CPU-owned AxVM timer wheel | hint only; callback never asserts a PPI |
| FDT interrupt identity | `GuestTimerProfile` | shared by runtime attachment and FDT generation |

No generic channel carries timer IRQ state. `IrqNotify` and the timer wheel
carry only deferred work or wake hints. A hard IRQ may acknowledge and
priority-drop its source and publish preallocated notification state; it must
not look up a VM, allocate, take `rdrive` locks, or invoke a subscriber.

An AArch64 hypervisor host enables split EOI mode in every GICv2 or GICv3 CPU
interface. `EOIR` therefore performs only the priority drop; `DIR` remains the
explicit completion boundary. Ordinary host IRQ dispatch preserves its
one-step behavior by performing both operations from `ActiveIrq::drop`.
AxVM-owned PPIs and SPIs use the same host mode but retain the acknowledged
activation until virtual retirement. A timer PPI retains its opaque token in
the timer binding. An assigned SPI names its physical source through the
hardware-backed LR instead of maintaining a second software pending state.
Enabling split EOI only in the AxVM fast path is invalid because the mode is
CPU-interface state shared with normal host IRQ dispatch.

An acknowledged interrupt that is not guest-owned still follows the ordinary
host IRQ graph. The raw GIC parent INTID is resolved through the installed
parent-to-leaf route before dispatch, so an ITS LPI reaches its MSI/MSI-X leaf
handler. GICv3 completion preserves the complete 24-bit architectural INTID
when issuing `DIR`; only the separately validated device-passthrough contract
is limited to assignable SPIs.

VGIC retirement invokes one typed backend operation after releasing the
controller lock. This is a single-owner lifecycle boundary, not a general
callback registry or a second pending queue.

## VM Configuration and Counter Domains

`ArmTimerVmConfig` contains:

- one guest-visible frequency;
- one VM-wide virtual offset;
- one physical offset.

AxVM records the hardware `CNTFRQ_EL0` value on every enabled physical CPU.
VM creation collects every CPU named by vCPU placement or affinity masks and
rejects:

- an empty target set;
- a target CPU without a published capability snapshot;
- zero counter frequency;
- heterogeneous counter frequencies.

If the host `arm,armv8-timer` node supplies a valid `clock-frequency`, that
firmware value is the guest-visible correction. It does not waive the
requirement that all target CPUs report one uniform hardware frequency.

The ordinary EL1 guest model is:

- `CNTVCT = CNTPCT - virtual_offset`;
- CNTV runs directly in hardware;
- CNTP/CNTPCT accesses trap and use `physical_offset`, currently zero;
- pause, deschedule, and restart do not rewrite the offset, so guest time keeps
  advancing;
- nested virtualization and guest EL2 timers are unsupported.

The stage-2 page-table level and IPA width use the minimum capability recorded
across all possible target CPUs. Timer and page-table selection therefore use
the same complete placement set and cannot silently fall back to the CPU that
happened to create the VM.

## World-Switch Transaction

### Guest entry

The final assembly-only window performs:

1. save host `CNTHCTL_EL2` and `CNTKCTL_EL1`;
2. write `CNTV_CTL_EL0 = 0`, then ISB;
3. install VM `CNTVOFF_EL2`;
4. install the guest timer trap policy and guest `CNTKCTL_EL1`;
5. install `CNTV_CVAL_EL0`, then ISB;
6. install writable `CNTV_CTL_EL0`;
7. mark the timer context loaded, then ISB;
8. restore guest registers and ERET without calling Rust.

VGIC state and the current pCPU timer-PPI route are prepared before this
transaction.

### Guest exit

For a lower-EL IRQ, the exception assembly first reads the host IAR while the
guest timer level is still asserted and stores only the raw acknowledgement in
the vCPU's host-runtime slot. GICv2 uses the immutable memory-mapped CPU
interface address discovered during VGIC construction; GICv3 uses
`ICC_IAR1_EL1`. This is an assembly-only operation: no Rust, VM lookup,
allocation, callback, or controller lock runs with guest timer state loaded.

The common exit transaction then performs:

1. read CNTV CTL and CVAL into the vCPU context;
2. advance the timer generation;
3. save guest `CNTKCTL_EL1`;
4. write `CNTV_CTL_EL0 = 0`, then ISB;
5. clear `CNTVOFF_EL2`;
6. restore host `CNTHCTL_EL2` and `CNTKCTL_EL1`;
7. mark the timer context unloaded, then ISB;
8. only then call Rust.

After the host timer context is restored, Rust validates the captured IAR
value, performs the split-EOI priority drop, and converts it into an opaque
completion token. It then publishes current CNTV/CNTP levels before saving and
merging VGIC state. If the exit is a trapped DIR, the guest's preceding
CVAL/CTL writes are therefore visible before deactivation decides whether a
level must repend.

Acknowledging after writing `CNTV_CTL_EL0 = 0` is invalid for a level PPI. The
line can deassert before `GICC_IAR` or `ICC_IAR1_EL1` is read, producing a
spurious INTID and an immediate re-entry loop. This ordering is shared by
GICv2, GICv3, QEMU, and real boards; it is not a platform workaround.

## Timer PPI Lifecycle

The virtual and non-secure physical timer INTIDs come from
`GuestTimerProfile`; they are not fixed constants in runtime code.

CNTV host interrupt handling is split:

1. AxVM claims the architectural virtual-timer PPI once for the hypervisor
   lifetime, configures it as level-triggered on every pCPU, and keeps one
   fixed allocation-free fallback for host-context races;
2. the host GIC CPU interface is already in split EOI mode;
3. lower-EL IRQ assembly acknowledges the CPU-local PPI before stopping CNTV;
4. post-switch Rust priority-drops the captured acknowledgement and records
   its opaque token plus owner pCPU;
5. timer snapshot publication updates the virtual PPI level;
6. VGIC owns delivery and guest enable/active state;
7. GICv2 EOI/DIR or GICv3 LR/TDIR retirement reaches the typed backend
   retirement boundary after the controller lock is released;
8. the host token is deactivated on its owner pCPU.

Lowering the timer line does not complete the host activation. This is
essential when a guest clears CVAL or CTL before writing DIR: the virtual line
may be low while its prior delivery is still architecturally active.

Migration may force completion on the old pCPU before loading the vCPU on a
new pCPU. Reset, stop, and drop likewise complete and discard the host
activation after invalidating timer-wheel work. These are explicit lifecycle
operations, not substitutes for ordinary guest retirement.

## Assigned Physical SPI Lifecycle

An assigned physical SPI uses an ownership-checked hardware-backed LR:

1. the host top half acknowledges the SPI and performs the priority drop;
2. VGIC queues one canonical delivery carrying the guest INTID and physical
   INTID;
3. GICv2 writes `HW` plus the physical ID into `GICH_LR`, while GICv3 writes
   `HW` and `PINTID` into `ICH_LR_EL2`;
4. ordinary guest completion retires the physical activation in the GIC
   hardware, so harvesting the disappeared LR must not issue another host
   `DIR`;
5. if a level source is still asserted, physical deactivation resamples it and
   produces a fresh host acknowledgement through the same ingress route.

A replacement host acknowledgement may arrive before software harvests the
stale LR snapshot. Delivery de-duplication must retain that acknowledgement
until refill can create a new HW-backed LR; otherwise a continuously asserted
device can lose its only replacement activation. A guest DIR that traps before
hardware retirement, and explicit rollback or teardown, use the typed backend
to issue host `DIR`. Sampling `GICD_ISPENDR` before `DIR` is not an equivalent
completion mechanism because physical deactivation is the architectural level
resample point.

## WFI, Timer Wheel, and Migration

For WFI, `ArmTimerSnapshot::earliest_deadline` considers both CNTV and CNTP.
Disabled, masked, or already-expired timers do not schedule a future wakeup.

Each scheduled callback carries:

- a vCPU timer generation;
- a `VmTimerHandle` containing the owner CPU and timer token.

The callback only validates its generation and wakes the vCPU. In task
context, the vCPU reads the current physical counter, republishes timer levels,
and rearms if the event arrived early. It never asserts a PPI based solely on a
timer-wheel callback.

Cancellation uses the handle's recorded owner CPU. Remote cancellation runs on
that owner and reprograms its one-shot comparator, preventing migration from
canceling the wrong CPU queue or accumulating long-lived stale events.

Reset advances, rather than reinitializes, each timer generation. A generation
value used before reset can therefore never become valid again merely because
the timer contexts were cleared.

## Firmware Contract

With a host FDT, AxVM requires a valid `arm,armv8-timer` node and parses:

- effective `interrupt-parent`;
- exactly four or five three-cell GIC PPI specifiers;
- level trigger flags, including retained legacy PPI CPU-mask bits;
- mandatory secure physical, non-secure physical, virtual, and hypervisor
  interrupt order;
- optional nonzero `clock-frequency`;
- optional timer phandle.

The guest FDT removes existing Arm timer nodes and creates one standard
`arm,armv8-timer` node. It preserves interrupt order, parent identity, raw
specifier flags, optional phandle, and an explicitly valid frequency. Host
errata and suspend properties are not copied.

Runtime vCPU attachment and FDT installation validate and consume the same
`GuestTimerProfile`; a developer-supplied DTB cannot introduce an independent
timer resource definition.

When there is no host FDT, the fixed QEMU machine profile supplies the standard
four PPIs. A present but malformed host timer node is an error; AxVM does not
guess board resources.

## Failure and Teardown

Unsupported or inconsistent timer capabilities fail VM construction. The
implementation does not:

- continue with the current CPU's frequency or IPA width;
- mask a PPI indefinitely as a substitute for deferred deactivate;
- convert a timer PPI into an assigned SPI;
- infer an IRQ number from a board name;
- preserve stale timer-wheel work across reset;
- retain a second timer device or relay compatibility path.

Registration is transactional. A failed duplicate timer-PPI registration does
not unregister the existing binding. Teardown removes the retirement route
before releasing the final binding and always attempts to complete an owned
host activation.

## Validation

Deterministic regressions cover:

- disabling CNTV and executing ISB before clearing CNTVOFF;
- complete entry/exit assembly operation ordering;
- lower-EL IAR acknowledgement before CNTV is stopped;
- CVAL/TVAL, ENABLE, IMASK, derived ISTATUS, wraparound, and reset generation;
- earliest CNTV/CNTP WFI deadline;
- uniform target-CPU frequency and minimum target-CPU capabilities;
- owner-aware timer cancellation and stale generation;
- GICv2 and GICv3 hypervisor host CPU interfaces both enabling split EOI;
- GICv2 EOI/DIR and GICv3 TDIR retirement;
- GICv2/GICv3 HW-backed assigned-SPI LR identity and normal hardware
  retirement without duplicate host deactivation;
- a replacement physical acknowledgement surviving a stale LR snapshot;
- trapped DIR always reaching physical deactivation before level resampling;
- unassigned GICv3 LPIs resolving to their MSI leaf and retaining the full
  24-bit INTID through host `DIR`;
- high-level re-pend after EOI and low-level non-repend;
- host timer-PPI completion only through VGIC retirement;
- four/five FDT interrupts, malformed cells, PPI class/trigger, frequency,
  parent, phandle, and interrupt order.

Before merge, the validation matrix must additionally complete:

- QEMU AArch64 GICv2 and GICv3 timer stress with SMP, WFI, and repeated sleeps;
- existing x86 VMX/SVM, RISC-V, and Phytium smoke;
- three consecutive RK3568 boots to the guest marker without epoch jumps;
- repeated RK3588/OrangePi-5-Plus success to guard the existing path;
- targeted `arm_vcpu`, `arm_vgic`, and `axvm` clippy with no new warnings;
- removal of all temporary timer/IRQ diagnostics.
