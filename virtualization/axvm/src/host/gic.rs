//! AArch64 GIC host operations for the ArceOS-backed AxVM runtime.

#[cfg(feature = "rt-trace")]
use core::sync::atomic::AtomicU64;
use core::sync::atomic::{AtomicUsize, Ordering};

#[cfg(feature = "rt-trace")]
use arm_gic_driver::v3::{
    ICH_AP0R0_EL2, ICH_AP0R1_EL2, ICH_AP0R2_EL2, ICH_AP0R3_EL2, ICH_AP1R0_EL2, ICH_AP1R1_EL2,
    ICH_AP1R2_EL2, ICH_AP1R3_EL2, ICH_EISR_EL2, ICH_MISR_EL2, ICH_VMCR_EL2,
};
use arm_gic_driver::{
    checked_intid,
    v3::{
        ICH_ELRSR_EL2, ICH_HCR_EL2, ICH_LR_EL2, ICH_VTR_EL2, ReadWriteable, Readable, Trigger, dir,
        eoi_mode, ich_lr_el2_get, ich_lr_el2_write,
    },
};
use ax_errno::{AxResult, ax_err_type};
use ax_memory_addr::{PhysAddr, VirtAddr};
use axvm_types::InterruptTriggerMode;

use super::{HostCpu, HostMemory, arceos, default_host};

static LR_EXHAUSTION_DIAGNOSTICS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "rt-trace")]
static VCPU_EXIT_GIC_DIAGNOSTICS: AtomicUsize = AtomicUsize::new(0);
const MAX_GICV3_LIST_REGISTERS: usize = 16;
#[cfg(feature = "rt-trace")]
const MAX_DIAGNOSTIC_CPUS: usize = 64;
const GIC_BACKEND_UNKNOWN: usize = 0;
const GIC_BACKEND_V2: usize = 2;
const GIC_BACKEND_V3: usize = 3;

static GIC_BACKEND: AtomicUsize = AtomicUsize::new(GIC_BACKEND_UNKNOWN);
static GIC_MAX_INTID: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "rt-trace")]
static LAST_VGIC_FINGERPRINT: [AtomicU64; MAX_DIAGNOSTIC_CPUS] =
    [const { AtomicU64::new(u64::MAX) }; MAX_DIAGNOSTIC_CPUS];
#[cfg(feature = "rt-trace")]
static VGIC_STABLE_EXIT_COUNT: [AtomicUsize; MAX_DIAGNOSTIC_CPUS] =
    [const { AtomicUsize::new(0) }; MAX_DIAGNOSTIC_CPUS];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ListRegisterState {
    Invalid,
    Pending,
    Active,
    PendingAndActive,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ListRegisterSnapshot {
    state: ListRegisterState,
    vector: usize,
    physical_irq: Option<usize>,
}

impl ListRegisterSnapshot {
    const EMPTY: Self = Self {
        state: ListRegisterState::Invalid,
        vector: 0,
        physical_irq: None,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ListRegisterSelection {
    Use(usize),
    Existing(usize),
    ExistingSoftware(usize),
    Conflict(usize),
    Deferred,
}

fn select_list_register(
    slots: &[ListRegisterSnapshot],
    vector: usize,
    physical_irq: Option<usize>,
) -> ListRegisterSelection {
    let mut free = None;
    for (index, slot) in slots.iter().enumerate() {
        if slot.state == ListRegisterState::Invalid {
            free.get_or_insert(index);
        } else if slot.vector == vector {
            return match physical_irq {
                None => ListRegisterSelection::Existing(index),
                Some(physical_irq) if slot.physical_irq == Some(physical_irq) => {
                    ListRegisterSelection::Existing(index)
                }
                Some(_) if slot.physical_irq.is_none() => {
                    ListRegisterSelection::ExistingSoftware(index)
                }
                Some(_) => ListRegisterSelection::Conflict(index),
            };
        }
    }
    free.map_or(ListRegisterSelection::Deferred, ListRegisterSelection::Use)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GicV3Injection {
    Injected {
        list_register: usize,
    },
    Existing {
        list_register: usize,
        state: ListRegisterState,
    },
    RepresentedBySoftware {
        list_register: usize,
        state: ListRegisterState,
    },
    Deferred,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VirtualIrqInjection {
    Injected,
    AlreadyPending,
    Deferred,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PhysicalIrqInjection {
    Injected,
    ReplayRequired,
    Deferred,
    Rejected,
}

fn classify_physical_irq_injection(injection: GicV3Injection) -> PhysicalIrqInjection {
    match injection {
        GicV3Injection::Injected { .. } => PhysicalIrqInjection::Injected,
        GicV3Injection::Existing { .. } | GicV3Injection::RepresentedBySoftware { .. } => {
            PhysicalIrqInjection::ReplayRequired
        }
        GicV3Injection::Deferred => PhysicalIrqInjection::Deferred,
    }
}

fn with_gic<T>(f: impl FnOnce(&mut rdif_intc::Intc) -> T) -> T {
    let mut gic = rdrive::get_one::<rdif_intc::Intc>()
        .expect("failed to get GIC driver")
        .lock()
        .expect("failed to lock GIC driver");
    f(&mut gic)
}

/// Caches immutable GIC capabilities before virtual IRQ forwarding starts.
///
/// GICv3 list registers are CPU-local system registers. Keeping their hot
/// injection path independent from the global `rdrive` device lock avoids a
/// lock re-entry when a physical IRQ preempts another virtual IRQ injection.
pub(crate) fn init_backend() {
    if GIC_BACKEND.load(Ordering::Acquire) != GIC_BACKEND_UNKNOWN {
        return;
    }

    let (backend, max_intid) = with_gic(|gic| {
        if let Some(gic) = gic.typed_mut::<arm_gic_driver::v2::Gic>() {
            return (GIC_BACKEND_V2, gic.max_intid() as usize);
        }
        if let Some(gic) = gic.typed_mut::<arm_gic_driver::v3::Gic>() {
            return (GIC_BACKEND_V3, gic.max_intid() as usize);
        }
        panic!("no GIC driver found");
    });

    GIC_MAX_INTID.store(max_intid, Ordering::Relaxed);
    GIC_BACKEND.store(backend, Ordering::Release);
    info!("AxVM cached GIC backend v{backend}, max_intid={max_intid}");
}

/// Configures the physical GIC trigger mode from authoritative device metadata.
pub(crate) fn configure_physical_irq_trigger(
    physical_irq: usize,
    trigger: InterruptTriggerMode,
) -> AxResult {
    if GIC_BACKEND.load(Ordering::Acquire) == GIC_BACKEND_UNKNOWN {
        init_backend();
    }

    let backend = GIC_BACKEND.load(Ordering::Acquire);
    let max_intid = GIC_MAX_INTID.load(Ordering::Relaxed) as u32;
    let intid = checked_intid(physical_irq as u32, max_intid).map_err(|_| {
        ax_err_type!(
            InvalidInput,
            alloc::format!("AArch64 physical IRQ {physical_irq} is outside the probed GIC range")
        )
    })?;
    let trigger = match trigger {
        InterruptTriggerMode::EdgeTriggered => Trigger::Edge,
        InterruptTriggerMode::LevelTriggered => Trigger::Level,
    };

    let configured = with_gic(|gic| match backend {
        GIC_BACKEND_V2 => gic
            .typed_mut::<arm_gic_driver::v2::Gic>()
            .is_some_and(|gic| {
                gic.set_cfg(intid, trigger);
                true
            }),
        GIC_BACKEND_V3 => gic
            .typed_mut::<arm_gic_driver::v3::Gic>()
            .is_some_and(|gic| {
                gic.set_cfg(intid, trigger);
                true
            }),
        _ => false,
    });
    if !configured {
        return Err(ax_err_type!(
            Unsupported,
            "registered interrupt controller is not a supported ARM GIC"
        ));
    }
    Ok(())
}

pub(crate) fn inject_interrupt(irq: usize) -> VirtualIrqInjection {
    debug!("Injecting virtual interrupt: {irq}");

    match GIC_BACKEND.load(Ordering::Acquire) {
        GIC_BACKEND_V2 => with_gic(|gic| {
            let Some(gic) = gic.typed_mut::<arm_gic_driver::v2::Gic>() else {
                panic!("cached GICv2 backend does not match the registered driver");
            };
            use arm_gic_driver::{
                IntId,
                v2::{VirtualInterruptConfig, VirtualInterruptState},
            };

            let gich = gic.hypervisor_interface().expect("failed to get GICH");
            gich.enable();
            gich.set_virtual_interrupt(
                0,
                VirtualInterruptConfig::software(
                    unsafe { IntId::raw(irq as _) },
                    None,
                    0,
                    VirtualInterruptState::Pending,
                    false,
                    true,
                ),
            );
            VirtualIrqInjection::Injected
        }),
        GIC_BACKEND_V3 => match inject_interrupt_gic_v3(irq, None) {
            GicV3Injection::Injected { .. } => VirtualIrqInjection::Injected,
            GicV3Injection::Existing { .. } | GicV3Injection::RepresentedBySoftware { .. } => {
                VirtualIrqInjection::AlreadyPending
            }
            GicV3Injection::Deferred => VirtualIrqInjection::Deferred,
        },
        GIC_BACKEND_UNKNOWN => panic!("GIC backend is not initialized"),
        backend => panic!("invalid cached GIC backend {backend}"),
    }
}

pub(crate) fn inject_routed_physical_interrupt(
    vector: usize,
    physical_irq: usize,
    priority: u8,
) -> PhysicalIrqInjection {
    if GIC_BACKEND.load(Ordering::Acquire) != GIC_BACKEND_V3 {
        return PhysicalIrqInjection::Rejected;
    }

    let max_intid = GIC_MAX_INTID.load(Ordering::Relaxed) as u32;
    if checked_intid(physical_irq as u32, max_intid).is_err() {
        return PhysicalIrqInjection::Rejected;
    }

    // The physical running priority was sampled immediately after IAR
    // acknowledgement. Reusing it here both preserves deliverability and
    // avoids acquiring the global GIC device lock in interrupt context.
    classify_physical_irq_injection(inject_interrupt_gic_v3(
        vector,
        Some((physical_irq, priority)),
    ))
}

/// Completes the physical deactivation retained by a previously forwarded
/// `ActiveIrq` after the guest interrupt was represented without a new HW LR.
pub(crate) fn deactivate_deferred_physical_interrupt(physical_irq: usize) -> bool {
    if GIC_BACKEND.load(Ordering::Acquire) != GIC_BACKEND_V3 {
        return false;
    }

    let max_intid = GIC_MAX_INTID.load(Ordering::Relaxed) as u32;
    let Ok(intid) = checked_intid(physical_irq as u32, max_intid) else {
        return false;
    };

    if eoi_mode() {
        dir(intid);
    }
    true
}

fn inject_interrupt_gic_v3(vector: usize, physical: Option<(usize, u8)>) -> GicV3Injection {
    debug!("Injecting virtual interrupt: vector={vector}");
    let elsr = ICH_ELRSR_EL2.read(ICH_ELRSR_EL2::STATUS);
    let lr_num = ICH_VTR_EL2.read(ICH_VTR_EL2::LISTREGS) as usize + 1;

    if lr_num > MAX_GICV3_LIST_REGISTERS {
        warn!(
            "GICv3 reports {lr_num} list registers, exceeding supported maximum \
             {MAX_GICV3_LIST_REGISTERS}"
        );
        return GicV3Injection::Deferred;
    }

    let mut slots = [ListRegisterSnapshot::EMPTY; MAX_GICV3_LIST_REGISTERS];
    for (index, slot) in slots[..lr_num].iter_mut().enumerate() {
        let lr = ich_lr_el2_get(index);
        let state = if lr.matches_all(ICH_LR_EL2::STATE::Invalid) {
            ListRegisterState::Invalid
        } else if lr.matches_all(ICH_LR_EL2::STATE::Pending) {
            ListRegisterState::Pending
        } else if lr.matches_all(ICH_LR_EL2::STATE::Active) {
            ListRegisterState::Active
        } else {
            ListRegisterState::PendingAndActive
        };
        *slot = ListRegisterSnapshot {
            state,
            vector: lr.read(ICH_LR_EL2::VINTID) as usize,
            physical_irq: lr
                .matches_all(ICH_LR_EL2::HW::SET)
                .then(|| lr.read(ICH_LR_EL2::PINTID) as usize),
        };
    }

    let physical_irq = physical.map(|(physical_irq, _)| physical_irq);
    let free_lr = match select_list_register(&slots[..lr_num], vector, physical_irq) {
        ListRegisterSelection::Use(index) => index,
        ListRegisterSelection::Existing(index) => {
            debug!("Virtual interrupt {vector} already owns LR{index}, physical={physical_irq:?}");
            return GicV3Injection::Existing {
                list_register: index,
                state: slots[index].state,
            };
        }
        ListRegisterSelection::ExistingSoftware(index) => {
            debug!("Physical interrupt {vector} is already represented by software LR{index}");
            return GicV3Injection::RepresentedBySoftware {
                list_register: index,
                state: slots[index].state,
            };
        }
        selection @ (ListRegisterSelection::Conflict(_) | ListRegisterSelection::Deferred) => {
            log_deferred_injection(vector, physical, elsr, &slots[..lr_num], selection);
            return GicV3Injection::Deferred;
        }
    };

    if !ICH_HCR_EL2.is_set(ICH_HCR_EL2::EN) {
        warn!("Virtual interrupt interface not enabled, enabling now");
        ICH_HCR_EL2.modify(ICH_HCR_EL2::EN::SET);
    }

    let mut lr =
        ICH_LR_EL2::VINTID.val(vector as u64) + ICH_LR_EL2::STATE::Pending + ICH_LR_EL2::GROUP::SET;
    if let Some((physical, priority)) = physical {
        lr += ICH_LR_EL2::HW::SET
            + ICH_LR_EL2::PINTID.val(physical as u64)
            + ICH_LR_EL2::PRIORITY.val(priority as u64);
    }
    ich_lr_el2_write(free_lr, lr);

    debug!(
        "Virtual interrupt {vector} injected successfully in LR{free_lr}, physical={physical:?}"
    );
    GicV3Injection::Injected {
        list_register: free_lr,
    }
}

fn log_deferred_injection(
    vector: usize,
    physical: Option<(usize, u8)>,
    elsr: u64,
    slots: &[ListRegisterSnapshot],
    selection: ListRegisterSelection,
) {
    let count = LR_EXHAUSTION_DIAGNOSTICS.fetch_add(1, Ordering::Relaxed);
    if count < 8 || count.is_multiple_of(1024) {
        warn!(
            "GICv3 IRQ injection deferred: cpu={}, vector={vector}, physical={physical:?}, \
             elsr={elsr:#x}, selection={selection:?}, slots={slots:?}, sample={}",
            default_host().this_cpu_id(),
            count + 1
        );
    }
}

#[cfg(feature = "rt-trace")]
fn active_priority_registers() -> ([u64; 4], [u64; 4]) {
    let preemption_bits = ICH_VTR_EL2.read(ICH_VTR_EL2::PREBITS) as usize + 1;
    let register_count = (1usize << preemption_bits.saturating_sub(5)).min(4);
    let mut group0 = [0u64; 4];
    let mut group1 = [0u64; 4];

    group0[0] = ICH_AP0R0_EL2.get();
    group1[0] = ICH_AP1R0_EL2.get();
    if register_count > 1 {
        group0[1] = ICH_AP0R1_EL2.get();
        group1[1] = ICH_AP1R1_EL2.get();
    }
    if register_count > 2 {
        group0[2] = ICH_AP0R2_EL2.get();
        group1[2] = ICH_AP1R2_EL2.get();
    }
    if register_count > 3 {
        group0[3] = ICH_AP0R3_EL2.get();
        group1[3] = ICH_AP1R3_EL2.get();
    }

    (group0, group1)
}

#[cfg(any(feature = "rt-trace", test))]
fn should_log_stable_vgic_state(has_candidate_state: bool, stable_exits: usize) -> bool {
    has_candidate_state && stable_exits >= 64 && stable_exits.is_power_of_two()
}

#[cfg(feature = "rt-trace")]
pub(crate) fn log_vcpu_exit_state(vm_id: usize, vcpu_id: usize) {
    if GIC_BACKEND.load(Ordering::Acquire) != GIC_BACKEND_V3 {
        return;
    }

    let cpu_id = default_host().this_cpu_id();
    if cpu_id >= MAX_DIAGNOSTIC_CPUS {
        return;
    }

    let hcr = ICH_HCR_EL2.get();
    let vmcr = ICH_VMCR_EL2.get();
    let misr = ICH_MISR_EL2.get();
    let eisr = ICH_EISR_EL2.get();
    let elrsr = ICH_ELRSR_EL2.get();
    let active_priorities = active_priority_registers();
    let lr_num =
        (ICH_VTR_EL2.read(ICH_VTR_EL2::LISTREGS) as usize + 1).min(MAX_GICV3_LIST_REGISTERS);
    let mut list_registers = [0u64; MAX_GICV3_LIST_REGISTERS];
    for (index, value) in list_registers[..lr_num].iter_mut().enumerate() {
        *value = ich_lr_el2_get(index).get();
    }

    let has_active_priority = active_priorities
        .0
        .iter()
        .chain(active_priorities.1.iter())
        .any(|value| *value != 0);
    let is_device_hardware_lr =
        |value: u64| value >> 62 != 0 && value & (1u64 << 61) != 0 && value & 0xffff_ffff >= 32;
    let has_device_hardware_lr = list_registers[..lr_num]
        .iter()
        .any(|value| is_device_hardware_lr(*value));
    if !has_active_priority && !has_device_hardware_lr {
        LAST_VGIC_FINGERPRINT[cpu_id].store(u64::MAX, Ordering::Relaxed);
        VGIC_STABLE_EXIT_COUNT[cpu_id].store(0, Ordering::Relaxed);
        return;
    }

    let mut fingerprint = hcr ^ vmcr.rotate_left(7) ^ misr.rotate_left(13) ^ eisr.rotate_left(19);
    for value in active_priorities.0.iter().chain(active_priorities.1.iter()) {
        fingerprint = fingerprint.rotate_left(5) ^ value;
    }
    for value in list_registers[..lr_num]
        .iter()
        .copied()
        .filter(|value| is_device_hardware_lr(*value))
    {
        fingerprint = fingerprint.rotate_left(5) ^ value;
    }

    let previous_fingerprint = LAST_VGIC_FINGERPRINT[cpu_id].swap(fingerprint, Ordering::Relaxed);
    let stable_exits = if previous_fingerprint == fingerprint {
        VGIC_STABLE_EXIT_COUNT[cpu_id].fetch_add(1, Ordering::Relaxed) + 1
    } else {
        VGIC_STABLE_EXIT_COUNT[cpu_id].store(0, Ordering::Relaxed);
        0
    };
    if !should_log_stable_vgic_state(has_active_priority || has_device_hardware_lr, stable_exits) {
        return;
    }

    let count = VCPU_EXIT_GIC_DIAGNOSTICS.fetch_add(1, Ordering::Relaxed) + 1;

    info!(
        "AICP_GIC_STUCK sample={count} stable_exits={stable_exits} cpu={cpu_id} vm={vm_id} \
         vcpu={vcpu_id} hcr={hcr:#x} vmcr={vmcr:#x} misr={misr:#x} eisr={eisr:#x} \
         elrsr={elrsr:#x} ap0r={:x?} ap1r={:x?} lrs={:?}",
        active_priorities.0,
        active_priorities.1,
        &list_registers[..lr_num],
    );
}

pub(crate) fn read_gicd_iidr() -> u32 {
    with_gic(|gic| {
        if let Some(gic) = gic.typed_mut::<arm_gic_driver::v2::Gic>() {
            return gic.iidr_raw();
        }
        if let Some(gic) = gic.typed_mut::<arm_gic_driver::v3::Gic>() {
            return gic.iidr_raw();
        }
        panic!("no GIC driver found");
    })
}

pub(crate) fn read_gicd_typer() -> u32 {
    with_gic(|gic| {
        if let Some(gic) = gic.typed_mut::<arm_gic_driver::v2::Gic>() {
            return gic.typer_raw();
        }
        if let Some(gic) = gic.typed_mut::<arm_gic_driver::v3::Gic>() {
            return gic.typer_raw();
        }
        panic!("no GIC driver found");
    })
}

pub(crate) fn host_gicd_base() -> PhysAddr {
    with_gic(|gic| {
        if let Some(gic) = gic.typed_mut::<arm_gic_driver::v2::Gic>() {
            return default_host().virt_to_phys(VirtAddr::from(usize::from(gic.gicd_addr())));
        }
        if let Some(gic) = gic.typed_mut::<arm_gic_driver::v3::Gic>() {
            return default_host().virt_to_phys(VirtAddr::from(usize::from(gic.gicd_addr())));
        }
        panic!("no GIC driver found");
    })
}

pub(crate) fn host_gicr_base() -> PhysAddr {
    with_gic(|gic| {
        if let Some(gic) = gic.typed_mut::<arm_gic_driver::v3::Gic>() {
            return default_host().virt_to_phys(VirtAddr::from(usize::from(gic.gicr_addr())));
        }
        panic!("no GICv3 driver found");
    })
}

pub(crate) fn handle_current_irq() -> Option<usize> {
    // AArch64 ArceOS platform IRQ handlers acknowledge the current IRQ
    // internally. The raw vector argument is ignored by current GIC-backed
    // platforms, so keep the ack/EOI ownership inside the platform handler.
    arceos::handle_host_irq(0)
}

pub(crate) fn handle_current_fiq() -> Option<usize> {
    arceos::handle_host_fiq(0)
}

pub(crate) fn fetch_irq() -> usize {
    handle_current_irq().unwrap_or(0)
}

pub(crate) fn fetch_fiq() -> usize {
    handle_current_fiq().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_priority_requires_a_stable_state_before_logging() {
        assert!(!should_log_stable_vgic_state(true, 0));
        assert!(!should_log_stable_vgic_state(true, 63));
        assert!(should_log_stable_vgic_state(true, 64));
        assert!(should_log_stable_vgic_state(true, 128));
        assert!(!should_log_stable_vgic_state(true, 129));
        assert!(!should_log_stable_vgic_state(false, 64));
    }

    #[test]
    fn physical_interrupt_does_not_reuse_lr_owned_by_another_source() {
        let slots = [ListRegisterSnapshot {
            state: ListRegisterState::Pending,
            vector: 27,
            physical_irq: Some(30),
        }];

        assert_eq!(
            select_list_register(&slots, 27, Some(27)),
            ListRegisterSelection::Conflict(0)
        );
    }

    #[test]
    fn physical_interrupt_recognizes_existing_software_representation() {
        let slots = [ListRegisterSnapshot {
            state: ListRegisterState::Pending,
            vector: 27,
            physical_irq: None,
        }];

        assert_eq!(
            select_list_register(&slots, 27, Some(27)),
            ListRegisterSelection::ExistingSoftware(0)
        );
    }

    #[test]
    fn full_list_register_bank_defers_injection() {
        let slots = [
            ListRegisterSnapshot {
                state: ListRegisterState::Active,
                vector: 32,
                physical_irq: None,
            },
            ListRegisterSnapshot {
                state: ListRegisterState::PendingAndActive,
                vector: 33,
                physical_irq: None,
            },
        ];

        assert_eq!(
            select_list_register(&slots, 27, Some(27)),
            ListRegisterSelection::Deferred
        );
    }

    #[test]
    fn existing_hardware_lr_requires_replay_for_a_new_physical_ack() {
        assert_eq!(
            classify_physical_irq_injection(GicV3Injection::Existing {
                list_register: 0,
                state: ListRegisterState::Active,
            }),
            PhysicalIrqInjection::ReplayRequired
        );
    }

    #[test]
    fn existing_software_lr_requires_replay_for_a_new_physical_ack() {
        assert_eq!(
            classify_physical_irq_injection(GicV3Injection::RepresentedBySoftware {
                list_register: 0,
                state: ListRegisterState::Pending,
            }),
            PhysicalIrqInjection::ReplayRequired
        );
    }
}
