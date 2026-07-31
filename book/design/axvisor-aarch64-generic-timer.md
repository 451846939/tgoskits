# Axvisor AArch64 Generic Timer Virtualization

## Status

This document defines the incompatible AArch64 generic-timer model used by
Axvisor. It is the ownership and world-switch contract for the implementation;
changes to timer state, counter offsets, PPI completion, vCPU migration, or
firmware resources must update this document before merge.

Implementation base: `origin/dev` at `024ecca10a4240a84b2c24bed2dc2361a6043d3e`.

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
| WFI wake event | CPU-owned AxVM timer wheel | hint only; callback never asserts a PPI |
| FDT interrupt identity | `GuestTimerProfile` | shared by runtime attachment and FDT generation |

No generic channel carries timer IRQ state. `IrqNotify` and the timer wheel
carry only deferred work or wake hints. A hard IRQ may acknowledge and
priority-drop its source and publish preallocated notification state; it must
not look up a VM, allocate, take `rdrive` locks, or invoke a subscriber.

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

The exception assembly performs:

1. read CNTV CTL and CVAL into the vCPU context;
2. advance the timer generation;
3. save guest `CNTKCTL_EL1`;
4. write `CNTV_CTL_EL0 = 0`, then ISB;
5. clear `CNTVOFF_EL2`;
6. restore host `CNTHCTL_EL2` and `CNTKCTL_EL1`;
7. mark the timer context unloaded, then ISB;
8. only then call Rust.

Rust then publishes current CNTV/CNTP levels before saving and merging VGIC
state. If the exit is a trapped DIR, the guest's preceding CVAL/CTL writes are
therefore visible before deactivation decides whether a level must repend.

## Timer PPI Lifecycle

The virtual and non-secure physical timer INTIDs come from
`GuestTimerProfile`; they are not fixed constants in runtime code.

CNTV host interrupt handling is split:

1. the host top half acknowledges and priority-drops the CPU-local PPI;
2. task context records the opaque token and its owner pCPU;
3. timer snapshot publication updates the virtual PPI level;
4. VGIC owns delivery and guest enable/active state;
5. GICv2 EOI/DIR or GICv3 LR/TDIR retirement reaches the typed backend
   retirement boundary after the controller lock is released;
6. the host token is deactivated on its owner pCPU.

Lowering the timer line does not complete the host activation. This is
essential when a guest clears CVAL or CTL before writing DIR: the virtual line
may be low while its prior delivery is still architecturally active.

Migration may force completion on the old pCPU before loading the vCPU on a
new pCPU. Reset, stop, and drop likewise complete and discard the host
activation after invalidating timer-wheel work. These are explicit lifecycle
operations, not substitutes for ordinary guest retirement.

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
- CVAL/TVAL, ENABLE, IMASK, derived ISTATUS, wraparound, and reset generation;
- earliest CNTV/CNTP WFI deadline;
- uniform target-CPU frequency and minimum target-CPU capabilities;
- owner-aware timer cancellation and stale generation;
- GICv2 EOI/DIR and GICv3 TDIR retirement;
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
