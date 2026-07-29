#[cfg(all(target_arch = "riscv64", feature = "hv"))]
use core::{
    hint::spin_loop,
    sync::atomic::{AtomicPtr, AtomicUsize, Ordering},
};

#[cfg(any(all(target_arch = "riscv64", feature = "hv"), test))]
use ax_plat::irq::IrqOutcome;
use ax_plat::irq::{
    CpuId, IrqAffinity, IrqError, IrqId, IrqIf, IrqSource, TrapVector, dispatch_irq_on,
};

#[cfg(all(target_arch = "loongarch64", feature = "hv"))]
mod loongarch64_hv;

#[cfg(all(target_arch = "riscv64", feature = "hv"))]
static VIRTUAL_IRQ_INJECTOR: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
#[cfg(all(target_arch = "riscv64", feature = "hv"))]
static VIRTUAL_IRQ_ROUTES: [RiscvVirtualIrqRouteSlot; RISCV_PLIC_SOURCE_COUNT] =
    [const { RiscvVirtualIrqRouteSlot::new() }; RISCV_PLIC_SOURCE_COUNT];
#[cfg(any(all(target_arch = "riscv64", feature = "hv"), test))]
const RISCV_PLIC_SOURCE_COUNT: usize = 1024;

#[cfg(all(target_arch = "riscv64", feature = "hv"))]
const UNASSIGNED_VIRTUAL_IRQ_ROUTE: usize = usize::MAX;

#[cfg(all(target_arch = "riscv64", feature = "hv"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RiscvVirtualIrqRoute {
    vm_id: usize,
    vcpu_id: usize,
    cpu_id: usize,
}

#[cfg(all(target_arch = "riscv64", feature = "hv"))]
struct RiscvVirtualIrqRouteSlot {
    sequence: AtomicUsize,
    vm_id: AtomicUsize,
    vcpu_id: AtomicUsize,
    cpu_id: AtomicUsize,
}

#[cfg(all(target_arch = "riscv64", feature = "hv"))]
impl RiscvVirtualIrqRouteSlot {
    const fn new() -> Self {
        Self {
            sequence: AtomicUsize::new(0),
            vm_id: AtomicUsize::new(UNASSIGNED_VIRTUAL_IRQ_ROUTE),
            vcpu_id: AtomicUsize::new(UNASSIGNED_VIRTUAL_IRQ_ROUTE),
            cpu_id: AtomicUsize::new(UNASSIGNED_VIRTUAL_IRQ_ROUTE),
        }
    }

    fn load(&self) -> Option<RiscvVirtualIrqRoute> {
        loop {
            let before = self.sequence.load(Ordering::Acquire);
            if before & 1 != 0 {
                spin_loop();
                continue;
            }
            let route = RiscvVirtualIrqRoute {
                vm_id: self.vm_id.load(Ordering::Relaxed),
                vcpu_id: self.vcpu_id.load(Ordering::Relaxed),
                cpu_id: self.cpu_id.load(Ordering::Relaxed),
            };
            let after = self.sequence.load(Ordering::Acquire);
            if before == after {
                return (route.vm_id != UNASSIGNED_VIRTUAL_IRQ_ROUTE).then_some(route);
            }
        }
    }

    fn store(&self, route: Option<RiscvVirtualIrqRoute>) {
        let sequence = self.lock_for_write();
        let route = route.unwrap_or(RiscvVirtualIrqRoute {
            vm_id: UNASSIGNED_VIRTUAL_IRQ_ROUTE,
            vcpu_id: UNASSIGNED_VIRTUAL_IRQ_ROUTE,
            cpu_id: UNASSIGNED_VIRTUAL_IRQ_ROUTE,
        });
        self.vcpu_id.store(route.vcpu_id, Ordering::Relaxed);
        self.cpu_id.store(route.cpu_id, Ordering::Relaxed);
        self.vm_id.store(route.vm_id, Ordering::Relaxed);
        self.sequence
            .store(sequence.wrapping_add(2), Ordering::Release);
    }

    fn clear_if_owned_by(&self, vm_id: usize) -> Option<RiscvVirtualIrqRoute> {
        let sequence = self.lock_for_write();
        let route = RiscvVirtualIrqRoute {
            vm_id: self.vm_id.load(Ordering::Relaxed),
            vcpu_id: self.vcpu_id.load(Ordering::Relaxed),
            cpu_id: self.cpu_id.load(Ordering::Relaxed),
        };
        let cleared = if route.vm_id == vm_id {
            self.vcpu_id
                .store(UNASSIGNED_VIRTUAL_IRQ_ROUTE, Ordering::Relaxed);
            self.cpu_id
                .store(UNASSIGNED_VIRTUAL_IRQ_ROUTE, Ordering::Relaxed);
            self.vm_id
                .store(UNASSIGNED_VIRTUAL_IRQ_ROUTE, Ordering::Relaxed);
            Some(route)
        } else {
            None
        };
        self.sequence
            .store(sequence.wrapping_add(2), Ordering::Release);
        cleared
    }

    fn lock_for_write(&self) -> usize {
        loop {
            let sequence = self.sequence.load(Ordering::Acquire);
            if sequence & 1 != 0 {
                spin_loop();
                continue;
            }
            if self
                .sequence
                .compare_exchange_weak(
                    sequence,
                    sequence.wrapping_add(1),
                    Ordering::Acquire,
                    Ordering::Relaxed,
                )
                .is_ok()
            {
                return sequence;
            }
        }
    }
}

#[cfg(all(target_arch = "riscv64", feature = "hv"))]
pub fn register_virtual_irq_injector(injector: fn(usize, usize, usize) -> bool) {
    VIRTUAL_IRQ_INJECTOR.store(injector as *mut (), Ordering::Release);
}

#[cfg(all(target_arch = "riscv64", feature = "hv"))]
pub fn set_virtual_irq_targets(vm_id: usize, vcpu_id: usize, cpu_id: usize, irq_sources: &[u32]) {
    for &irq in irq_sources {
        let irq = irq as usize;
        if irq == 0 || irq >= RISCV_PLIC_SOURCE_COUNT {
            warn!("skip invalid RISC-V guest PLIC source {irq}");
            continue;
        }
        let route = RiscvVirtualIrqRoute {
            vm_id,
            vcpu_id,
            cpu_id,
        };
        if let Some(previous) = VIRTUAL_IRQ_ROUTES[irq].load()
            && previous.vm_id != vm_id
        {
            warn!(
                "RISC-V PLIC source {irq} moved from VM[{}] VCpu[{}] to VM[{vm_id}] \
                 VCpu[{vcpu_id}]",
                previous.vm_id, previous.vcpu_id
            );
        }
        VIRTUAL_IRQ_ROUTES[irq].store(Some(route));
        route_virtual_irq_to_target_cpu(irq, cpu_id);
    }
}

#[cfg(all(target_arch = "riscv64", feature = "hv"))]
pub fn clear_virtual_irq_targets(vm_id: usize) {
    for (irq, route) in VIRTUAL_IRQ_ROUTES.iter().enumerate().skip(1) {
        if route.clear_if_owned_by(vm_id).is_some() {
            restore_virtual_irq_affinity(irq);
        }
    }
}

struct IrqIfImpl;

#[impl_plat_interface]
impl IrqIf for IrqIfImpl {
    fn prepare(_vector: TrapVector) {}

    fn init_boot_irqs(cpu_id: usize) -> Result<(), IrqError> {
        somehal::irq::init_boot_irqs(cpu_id)
    }

    #[cfg(feature = "smp")]
    fn init_secondary_boot_irqs(cpu_id: usize) -> Result<(), IrqError> {
        somehal::irq::init_secondary_boot_irqs(cpu_id);
        Ok(())
    }

    /// Enables or disables the given IRQ.
    fn set_enable(irq: IrqId, enabled: bool) -> Result<(), IrqError> {
        somehal::irq::irq_set_enable(irq, enabled)
    }

    fn set_affinity(irq: IrqId, affinity: IrqAffinity) -> Result<(), IrqError> {
        let affinity = match affinity {
            IrqAffinity::Any => somehal::irq::IrqAffinity::Any,
            IrqAffinity::Fixed(cpu) => somehal::irq::IrqAffinity::Fixed { cpu_id: cpu.0 },
        };
        somehal::irq::irq_set_affinity(irq, affinity)
    }

    /// Handles the IRQ.
    fn handle(vector: TrapVector) -> Option<IrqId> {
        let irq = {
            let active = somehal::irq::begin_irq(vector.0)?;
            let irq = active.id();

            #[cfg(all(target_arch = "riscv64", feature = "hv"))]
            if should_forward_riscv_guest_irq(irq, IrqOutcome::default())
                && inject_virtual_irq(irq.hwirq.0 as usize)
            {
                return Some(irq);
            }

            let cpu = current_irq_cpu();
            let outcome = dispatch_irq_on(irq, cpu);
            if !outcome.handled {
                #[cfg(all(target_arch = "loongarch64", feature = "hv"))]
                if is_loongarch_guest_forwardable(irq)
                    && loongarch64_hv::inject_virtual_irq(irq.hwirq.0 as usize)
                {
                    return Some(irq);
                }

                if outcome.called == 0 {
                    warn!("Unhandled IRQ {irq:?} on CPU {}", cpu.0);
                } else {
                    debug!("Spurious IRQ {irq:?}");
                }
            }
            irq
        };
        Some(irq)
    }

    fn send_ipi(id: IrqId, target: ax_plat::irq::IpiTarget) {
        let target = match target {
            ax_plat::irq::IpiTarget::Current { cpu_id } => {
                somehal::irq::IpiTarget::Current { cpu_id }
            }
            ax_plat::irq::IpiTarget::Other { cpu_id } => somehal::irq::IpiTarget::Other { cpu_id },
            ax_plat::irq::IpiTarget::AllExceptCurrent { cpu_id, cpu_num } => {
                somehal::irq::IpiTarget::AllExceptCurrent { cpu_id, cpu_num }
            }
        };
        somehal::irq::send_ipi(id, target);
    }

    fn ipi_irq() -> IrqId {
        somehal::irq::ipi_irq()
    }

    fn resolve_source(source: IrqSource) -> Result<IrqId, IrqError> {
        somehal::irq::resolve_irq_source(source)
    }

    fn resolve_percpu(hwirq: ax_plat::irq::HwIrq) -> Result<IrqId, IrqError> {
        #[cfg(target_arch = "aarch64")]
        {
            somehal::irq::aarch64_gic_irq_id_checked(hwirq)
        }
        #[cfg(any(target_arch = "loongarch64", target_arch = "x86_64"))]
        {
            Ok(IrqId::new(somehal::irq::CPU_LOCAL_IRQ_DOMAIN, hwirq))
        }
        #[cfg(target_arch = "riscv64")]
        {
            Ok(IrqId::new(somehal::irq::CPU_LOCAL_IRQ_DOMAIN, hwirq))
        }
    }
}

fn current_irq_cpu() -> CpuId {
    CpuId(ax_plat::percpu::this_cpu_id())
}

#[cfg(any(all(target_arch = "riscv64", feature = "hv"), test))]
fn is_guest_forwardable(irq: IrqId) -> bool {
    somehal::irq::domain_is_kind(irq.domain, somehal::irq::IrqDomainKind::RiscvPlic)
}

#[cfg(any(all(target_arch = "riscv64", feature = "hv"), test))]
fn should_forward_riscv_guest_irq(irq: IrqId, _host_outcome: IrqOutcome) -> bool {
    is_guest_forwardable(irq)
}

#[cfg(test)]
fn riscv_plic_source_index(irq: IrqId) -> Option<usize> {
    if !is_guest_forwardable(irq) {
        return None;
    }
    let source = irq.hwirq.0 as usize;
    (1..RISCV_PLIC_SOURCE_COUNT)
        .contains(&source)
        .then_some(source)
}

#[cfg(all(target_arch = "loongarch64", feature = "hv"))]
fn is_loongarch_guest_forwardable(irq: IrqId) -> bool {
    somehal::irq::domain_is_kind(irq.domain, somehal::irq::IrqDomainKind::LoongArchEioIntc)
        || somehal::irq::domain_is_kind(irq.domain, somehal::irq::IrqDomainKind::LoongArchPchPic)
}

#[cfg(all(target_arch = "riscv64", feature = "hv"))]
fn inject_virtual_irq(irq: usize) -> bool {
    let Some(route) = VIRTUAL_IRQ_ROUTES
        .get(irq)
        .and_then(RiscvVirtualIrqRouteSlot::load)
    else {
        trace!("skip RISC-V virtual IRQ {irq}: no guest route");
        return false;
    };
    route_virtual_irq_to_target_cpu(irq, route.cpu_id);

    let injector = VIRTUAL_IRQ_INJECTOR.load(Ordering::Acquire);
    if injector.is_null() {
        trace!("skip RISC-V virtual IRQ {irq}: injector is not registered");
        return false;
    }
    unsafe {
        // SAFETY: `register_virtual_irq_injector` stores only function pointers
        // with this exact signature, and Release/Acquire publishes the pointer.
        core::mem::transmute::<*mut (), fn(usize, usize, usize) -> bool>(injector)(
            route.vm_id,
            route.vcpu_id,
            irq,
        )
    }
}

#[cfg(all(target_arch = "riscv64", feature = "hv"))]
fn route_virtual_irq_to_target_cpu(irq: usize, target_cpu: usize) {
    if irq == 0 || irq >= RISCV_PLIC_SOURCE_COUNT {
        return;
    }

    let Some(domain) = somehal::irq::domain_by_kind_fast(somehal::irq::IrqDomainKind::RiscvPlic)
    else {
        trace!("skip RISC-V virtual IRQ {irq} affinity: PLIC domain is not registered");
        return;
    };
    let irq_id = IrqId::new(domain, ax_plat::irq::HwIrq(irq as u32));
    let affinity = somehal::irq::IrqAffinity::Fixed { cpu_id: target_cpu };
    if let Err(err) = somehal::irq::irq_set_affinity(irq_id, affinity) {
        trace!("skip RISC-V virtual IRQ {irq} affinity to CPU {target_cpu}: {err:?}");
    }
}

#[cfg(all(target_arch = "riscv64", feature = "hv"))]
fn restore_virtual_irq_affinity(irq: usize) {
    let Some(domain) = somehal::irq::domain_by_kind_fast(somehal::irq::IrqDomainKind::RiscvPlic)
    else {
        return;
    };
    let irq_id = IrqId::new(domain, ax_plat::irq::HwIrq(irq as u32));
    if let Err(err) = somehal::irq::irq_set_affinity(irq_id, somehal::irq::IrqAffinity::Any) {
        trace!("failed to restore RISC-V virtual IRQ {irq} affinity: {err:?}");
    }
}

#[cfg(test)]
mod tests {
    use ax_plat::irq::{CPU_LOCAL_IRQ_DOMAIN, HwIrq, IrqId};
    use spin::Once;

    fn plic_irq(hwirq: u32) -> IrqId {
        static PLIC_DOMAIN: Once<somehal::irq::IrqDomainId> = Once::new();

        let domain = *PLIC_DOMAIN.call_once(|| {
            somehal::irq::domain_by_kind(somehal::irq::IrqDomainKind::RiscvPlic)
                .map(|domain| domain.id)
                .unwrap_or_else(|| {
                    somehal::irq::alloc_irq_domain(
                        rdrive::DeviceId::new(),
                        somehal::irq::IrqDomainKind::RiscvPlic,
                    )
                    .unwrap()
                })
        });
        IrqId::new(domain, HwIrq(hwirq))
    }

    #[test]
    fn cpu_local_irq_is_never_forwarded_to_guest() {
        let irq = IrqId::new(CPU_LOCAL_IRQ_DOMAIN, HwIrq(5));

        assert!(!super::is_guest_forwardable(irq));
    }

    #[test]
    fn plic_irq_can_be_forwarded_to_guest() {
        let irq = plic_irq(10);

        assert!(super::is_guest_forwardable(irq));
    }

    #[test]
    fn handled_plic_irq_remains_forwardable_to_passthrough_guest() {
        let irq = plic_irq(1);
        let host_outcome = ax_plat::irq::IrqOutcome {
            handled: true,
            wake: false,
            called: 1,
        };

        assert!(super::should_forward_riscv_guest_irq(irq, host_outcome));
    }

    #[test]
    fn unhandled_plic_irq_can_be_forwarded_to_guest() {
        let irq = plic_irq(2);

        assert!(super::should_forward_riscv_guest_irq(
            irq,
            ax_plat::irq::IrqOutcome::default()
        ));
    }

    #[test]
    fn only_real_plic_sources_have_virtual_irq_source_index() {
        let irq = plic_irq(2);
        assert_eq!(super::riscv_plic_source_index(irq), Some(2));

        let reserved = IrqId::new(irq.domain, HwIrq(0));
        assert_eq!(super::riscv_plic_source_index(reserved), None);

        let out_of_range = IrqId::new(irq.domain, HwIrq(super::RISCV_PLIC_SOURCE_COUNT as u32));
        assert_eq!(super::riscv_plic_source_index(out_of_range), None);
    }
}
