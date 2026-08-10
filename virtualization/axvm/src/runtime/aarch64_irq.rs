//! AArch64 physical IRQ ownership and guest routing.

use core::sync::atomic::{AtomicU64, Ordering};

use ax_errno::{AxResult, ax_err_type};
use axvm_types::InterruptTriggerMode;

const MAX_GIC_INTIDS: usize = 1024;
const MAX_PHYSICAL_CPUS: usize = 256;
const VALID_BIT: u64 = 1 << 63;
const FIELD_MASK: u64 = 0xffff;
const TARGET_CPU_MASK: u64 = 0x1ff;
const NO_TARGET_CPU: usize = 0x100;

/// A physical interrupt route owned by one guest vCPU.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GuestIrqRoute {
    pub vm_id: usize,
    pub vcpu_id: usize,
    pub guest_vector: usize,
    pub target_pcpu: Option<usize>,
}

/// The guest vCPU that exclusively owns private interrupts delivered to a pCPU.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GuestPrivateIrqTarget {
    pub vm_id: usize,
    pub vcpu_id: usize,
}

struct RouteTable<const N: usize> {
    entries: [AtomicU64; N],
}

impl<const N: usize> RouteTable<N> {
    const fn new() -> Self {
        Self {
            entries: [const { AtomicU64::new(0) }; N],
        }
    }

    fn register(&self, physical_irq: usize, route: GuestIrqRoute) -> AxResult<bool> {
        let entry = self
            .entries
            .get(physical_irq)
            .ok_or_else(|| ax_err_type!(InvalidInput, "AArch64 physical IRQ is out of range"))?;
        let encoded = encode_route(route)?;

        loop {
            let current = entry.load(Ordering::Acquire);
            if current == encoded {
                return Ok(false);
            }
            if current != 0 {
                let owner = decode_route(current).expect("non-zero IRQ route must be valid");
                return Err(ax_err_type!(
                    AlreadyExists,
                    alloc::format!(
                        "physical IRQ {physical_irq} is already owned by VM[{}] VCpu[{}]",
                        owner.vm_id,
                        owner.vcpu_id
                    )
                ));
            }
            if entry
                .compare_exchange(0, encoded, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Ok(true);
            }
        }
    }

    fn lookup(&self, physical_irq: usize) -> Option<GuestIrqRoute> {
        self.entries
            .get(physical_irq)
            .and_then(|entry| decode_route(entry.load(Ordering::Acquire)))
    }

    fn unregister_vm(&self, vm_id: usize) -> usize {
        let mut removed = 0;
        for entry in &self.entries {
            loop {
                let current = entry.load(Ordering::Acquire);
                let Some(route) = decode_route(current) else {
                    break;
                };
                if route.vm_id != vm_id {
                    break;
                }
                if entry
                    .compare_exchange(current, 0, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    removed += 1;
                    break;
                }
            }
        }
        removed
    }
}

static ROUTES: RouteTable<MAX_GIC_INTIDS> = RouteTable::new();

struct PrivateIrqTargetTable<const N: usize> {
    entries: [AtomicU64; N],
}

impl<const N: usize> PrivateIrqTargetTable<N> {
    const fn new() -> Self {
        Self {
            entries: [const { AtomicU64::new(0) }; N],
        }
    }

    fn register(&self, pcpu_id: usize, target: GuestPrivateIrqTarget) -> AxResult<bool> {
        let entry = self.entries.get(pcpu_id).ok_or_else(|| {
            ax_err_type!(InvalidInput, "AArch64 private IRQ pCPU is out of range")
        })?;
        let encoded = encode_private_irq_target(target)?;

        loop {
            let current = entry.load(Ordering::Acquire);
            if current == encoded {
                return Ok(false);
            }
            if current != 0 {
                let owner = decode_private_irq_target(current)
                    .expect("non-zero private IRQ target must be valid");
                return Err(ax_err_type!(
                    AlreadyExists,
                    alloc::format!(
                        "pCPU {pcpu_id} private IRQs are already owned by VM[{}] VCpu[{}]",
                        owner.vm_id,
                        owner.vcpu_id
                    )
                ));
            }
            if entry
                .compare_exchange(0, encoded, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Ok(true);
            }
        }
    }

    fn lookup(&self, pcpu_id: usize) -> Option<GuestPrivateIrqTarget> {
        self.entries
            .get(pcpu_id)
            .and_then(|entry| decode_private_irq_target(entry.load(Ordering::Acquire)))
    }

    fn unregister_vm(&self, vm_id: usize) -> usize {
        let mut removed = 0;
        for entry in &self.entries {
            loop {
                let current = entry.load(Ordering::Acquire);
                let Some(target) = decode_private_irq_target(current) else {
                    break;
                };
                if target.vm_id != vm_id {
                    break;
                }
                if entry
                    .compare_exchange(current, 0, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    removed += 1;
                    break;
                }
            }
        }
        removed
    }
}

static PRIVATE_IRQ_TARGETS: PrivateIrqTargetTable<MAX_PHYSICAL_CPUS> = PrivateIrqTargetTable::new();

/// Register one physical GIC INTID as being owned by a guest vCPU.
///
/// Returns `true` for a newly installed route and `false` for an identical,
/// idempotent registration. Conflicting ownership is rejected.
pub fn register_guest_irq_route(
    physical_irq: usize,
    vm_id: usize,
    vcpu_id: usize,
    guest_vector: usize,
    target_pcpu: Option<usize>,
    trigger: InterruptTriggerMode,
) -> AxResult<bool> {
    crate::host::gic::configure_physical_irq_trigger(physical_irq, trigger)?;
    ROUTES.register(
        physical_irq,
        GuestIrqRoute {
            vm_id,
            vcpu_id,
            guest_vector,
            target_pcpu,
        },
    )
}

/// Remove every physical IRQ route owned by one VM.
pub fn unregister_guest_irq_routes(vm_id: usize) -> usize {
    ROUTES.unregister_vm(vm_id)
}

/// Return the registered owner of one physical GIC INTID.
pub(crate) fn guest_irq_route(physical_irq: usize) -> Option<GuestIrqRoute> {
    ROUTES.lookup(physical_irq)
}

/// Register the guest vCPU that exclusively owns private IRQs on one pCPU.
///
/// This mapping is derived from a fixed one-pCPU vCPU affinity. Ambiguous or
/// shared affinity must be rejected by the caller instead of guessing a guest.
pub fn register_guest_private_irq_target(
    pcpu_id: usize,
    vm_id: usize,
    vcpu_id: usize,
) -> AxResult<bool> {
    PRIVATE_IRQ_TARGETS.register(pcpu_id, GuestPrivateIrqTarget { vm_id, vcpu_id })
}

/// Remove every private IRQ pCPU target owned by one VM.
pub fn unregister_guest_private_irq_targets(vm_id: usize) -> usize {
    PRIVATE_IRQ_TARGETS.unregister_vm(vm_id)
}

/// Return the guest vCPU that owns private IRQs delivered to one pCPU.
pub(crate) fn guest_private_irq_target(pcpu_id: usize) -> Option<GuestPrivateIrqTarget> {
    PRIVATE_IRQ_TARGETS.lookup(pcpu_id)
}

fn encode_route(route: GuestIrqRoute) -> AxResult<u64> {
    for (name, value) in [
        ("VM id", route.vm_id),
        ("vCPU id", route.vcpu_id),
        ("guest vector", route.guest_vector),
    ] {
        if value > FIELD_MASK as usize {
            return Err(ax_err_type!(
                InvalidInput,
                alloc::format!("AArch64 IRQ route {name} {value} is out of range")
            ));
        }
    }

    let target_pcpu = route.target_pcpu.unwrap_or(NO_TARGET_CPU);
    if target_pcpu > NO_TARGET_CPU || route.target_pcpu == Some(NO_TARGET_CPU) {
        return Err(ax_err_type!(
            InvalidInput,
            "AArch64 IRQ route target pCPU is out of range"
        ));
    }

    Ok(VALID_BIT
        | route.vm_id as u64
        | ((route.vcpu_id as u64) << 16)
        | ((route.guest_vector as u64) << 32)
        | ((target_pcpu as u64) << 48))
}

fn decode_route(encoded: u64) -> Option<GuestIrqRoute> {
    if encoded & VALID_BIT == 0 {
        return None;
    }
    let target_pcpu = ((encoded >> 48) & TARGET_CPU_MASK) as usize;
    Some(GuestIrqRoute {
        vm_id: (encoded & FIELD_MASK) as usize,
        vcpu_id: ((encoded >> 16) & FIELD_MASK) as usize,
        guest_vector: ((encoded >> 32) & FIELD_MASK) as usize,
        target_pcpu: (target_pcpu != NO_TARGET_CPU).then_some(target_pcpu),
    })
}

fn encode_private_irq_target(target: GuestPrivateIrqTarget) -> AxResult<u64> {
    for (name, value) in [("VM id", target.vm_id), ("vCPU id", target.vcpu_id)] {
        if value > FIELD_MASK as usize {
            return Err(ax_err_type!(
                InvalidInput,
                alloc::format!("AArch64 private IRQ target {name} {value} is out of range")
            ));
        }
    }

    Ok(VALID_BIT | target.vm_id as u64 | ((target.vcpu_id as u64) << 16))
}

fn decode_private_irq_target(encoded: u64) -> Option<GuestPrivateIrqTarget> {
    if encoded & VALID_BIT == 0 {
        return None;
    }
    Some(GuestPrivateIrqTarget {
        vm_id: (encoded & FIELD_MASK) as usize,
        vcpu_id: ((encoded >> 16) & FIELD_MASK) as usize,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(vm_id: usize, vcpu_id: usize) -> GuestIrqRoute {
        GuestIrqRoute {
            vm_id,
            vcpu_id,
            guest_vector: 77,
            target_pcpu: Some(2),
        }
    }

    #[test]
    fn register_lookup_and_unregister() {
        let table = RouteTable::<128>::new();
        assert_eq!(table.lookup(77), None);
        assert!(table.register(77, route(1, 0)).unwrap());
        assert_eq!(table.lookup(77), Some(route(1, 0)));
        assert_eq!(table.unregister_vm(1), 1);
        assert_eq!(table.lookup(77), None);
    }

    #[test]
    fn identical_registration_is_idempotent() {
        let table = RouteTable::<128>::new();
        assert!(table.register(77, route(1, 0)).unwrap());
        assert!(!table.register(77, route(1, 0)).unwrap());
    }

    #[test]
    fn conflicting_registration_is_rejected() {
        let table = RouteTable::<128>::new();
        assert!(table.register(77, route(1, 0)).unwrap());
        assert!(table.register(77, route(2, 0)).is_err());
        assert_eq!(table.lookup(77), Some(route(1, 0)));
    }

    #[test]
    fn out_of_range_irq_is_rejected() {
        let table = RouteTable::<4>::new();
        assert!(table.register(4, route(1, 0)).is_err());
        assert_eq!(table.lookup(4), None);
    }

    #[test]
    fn private_irq_target_register_lookup_and_unregister() {
        let table = PrivateIrqTargetTable::<8>::new();
        let target = GuestPrivateIrqTarget {
            vm_id: 3,
            vcpu_id: 1,
        };

        assert!(table.register(4, target).unwrap());
        assert_eq!(table.lookup(4), Some(target));
        assert_eq!(table.unregister_vm(3), 1);
        assert_eq!(table.lookup(4), None);
    }

    #[test]
    fn identical_private_irq_target_registration_is_idempotent() {
        let table = PrivateIrqTargetTable::<8>::new();
        let target = GuestPrivateIrqTarget {
            vm_id: 3,
            vcpu_id: 1,
        };

        assert!(table.register(4, target).unwrap());
        assert!(!table.register(4, target).unwrap());
    }

    #[test]
    fn conflicting_private_irq_target_is_rejected() {
        let table = PrivateIrqTargetTable::<8>::new();
        let first = GuestPrivateIrqTarget {
            vm_id: 3,
            vcpu_id: 1,
        };
        let second = GuestPrivateIrqTarget {
            vm_id: 4,
            vcpu_id: 0,
        };

        assert!(table.register(4, first).unwrap());
        assert!(table.register(4, second).is_err());
        assert_eq!(table.lookup(4), Some(first));
    }

    #[test]
    fn out_of_range_private_irq_pcpu_is_rejected() {
        let table = PrivateIrqTargetTable::<4>::new();
        let target = GuestPrivateIrqTarget {
            vm_id: 3,
            vcpu_id: 1,
        };

        assert!(table.register(4, target).is_err());
        assert_eq!(table.lookup(4), None);
    }
}
