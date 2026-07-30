//! IRQ-safe ingress for guest-owned physical GIC SPIs.
//!
//! The host top half acknowledges and priority-drops the interrupt before it
//! enters this module.  It may only publish into a preallocated route slot;
//! VM/controller lookup and canonical VGIC state mutation run in the worker.

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::{
    hint::spin_loop,
    ptr,
    sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicUsize, Ordering},
};

use arm_vgic::{GicV3BackendError, PhysicalIrqId, VgicCore};
use ax_kspin::SpinNoIrq;
use ax_std::os::arceos::modules::ax_task::IrqNotify;
use axdevice_base::HostIrqId;

use super::{deactivate_host_irq, dispatch_acknowledged_host_irq, host_irq_intid};
use crate::{AxTaskRef, TaskInner};

const GIC_INTID_COUNT: usize = 1020;
const NO_HOST_TOKEN: usize = usize::MAX;
const DELIVERY_IDLE: u8 = 0;
const DELIVERY_RESERVING: u8 = 1;
const DELIVERY_INGRESS: u8 = 2;
const DELIVERY_ACTIVE: u8 = 3;
const PHYSICAL_SPI_WORKER_STACK_SIZE: usize = 0x20_000;

static ASSIGNED_SPI_ROUTES: [AssignedSpiRouteSlot; GIC_INTID_COUNT] =
    [const { AssignedSpiRouteSlot::new() }; GIC_INTID_COUNT];

/// Owns every fixed host-INTID route installed for one VM.
pub(crate) struct AssignedSpiRoutes {
    shared: Arc<AssignedSpiShared>,
    bindings: Box<[Arc<AssignedSpiBinding>]>,
    registrations: SpinNoIrq<Vec<AssignedSpiRouteRegistration>>,
    worker: SpinNoIrq<Option<AxTaskRef>>,
    running: AtomicBool,
}

impl AssignedSpiRoutes {
    pub(super) fn register(controller: &Arc<VgicCore>) -> Result<Arc<Self>, GicV3BackendError> {
        let shared = Arc::new(AssignedSpiShared {
            controller: controller.clone(),
            notify: IrqNotify::new(),
            stopping: AtomicBool::new(false),
        });
        let bindings = controller
            .config()
            .assigned_spis()
            .iter()
            .map(|assigned| {
                Arc::new(AssignedSpiBinding {
                    irq: assigned.host_irq(),
                    shared: shared.clone(),
                    accepting: AtomicBool::new(false),
                    delivery: AtomicU8::new(DELIVERY_IDLE),
                    host_token: AtomicUsize::new(NO_HOST_TOKEN),
                })
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        let routes = Arc::new(Self {
            shared,
            bindings,
            registrations: SpinNoIrq::new(Vec::new()),
            worker: SpinNoIrq::new(None),
            running: AtomicBool::new(false),
        });

        routes.start_worker();
        {
            let mut registrations = routes.registrations.lock();
            for binding in &routes.bindings {
                match AssignedSpiRouteRegistration::install(binding) {
                    Ok(registration) => registrations.push(registration),
                    Err(error) => {
                        registrations.clear();
                        drop(registrations);
                        routes.stop_worker();
                        return Err(error);
                    }
                }
            }
        }
        for binding in &routes.bindings {
            binding.accepting.store(true, Ordering::Release);
        }
        Ok(routes)
    }

    /// Stops accepting new activations and drains task-context ingress.
    ///
    /// Route slots remain installed so a late acknowledged IRQ is consumed
    /// instead of escaping to a host driver after ownership has transferred.
    pub(crate) fn quiesce(&self) {
        for binding in &self.bindings {
            binding.accepting.store(false, Ordering::Release);
        }
        self.stop_worker();
        for binding in &self.bindings {
            binding.release_unforwarded_activation();
        }
    }

    /// Restores ingress after a control-plane teardown attempt was rejected.
    pub(crate) fn resume(self: &Arc<Self>) {
        self.start_worker();
        for binding in &self.bindings {
            binding.accepting.store(true, Ordering::Release);
        }
    }

    fn start_worker(self: &Arc<Self>) {
        if self.running.swap(true, Ordering::AcqRel) {
            return;
        }
        self.shared.stopping.store(false, Ordering::Release);
        let routes = self.clone();
        let task = TaskInner::new(
            move || routes.run_worker(),
            "aarch64-physical-spi".into(),
            PHYSICAL_SPI_WORKER_STACK_SIZE,
        );
        *self.worker.lock() = Some(crate::host::task::spawn_task(task));
    }

    fn stop_worker(&self) {
        if !self.running.swap(false, Ordering::AcqRel) {
            return;
        }
        self.shared.stopping.store(true, Ordering::Release);
        self.shared.notify.notify();
        if let Some(worker) = self.worker.lock().take() {
            worker.join();
        }
    }

    fn run_worker(&self) {
        loop {
            self.shared.notify.wait();
            if self.shared.stopping.load(Ordering::Acquire) {
                break;
            }
            for binding in &self.bindings {
                binding.drain_ingress();
            }
        }
    }
}

impl Drop for AssignedSpiRoutes {
    fn drop(&mut self) {
        self.quiesce();
        self.registrations.lock().clear();
    }
}

struct AssignedSpiShared {
    controller: Arc<VgicCore>,
    notify: IrqNotify,
    stopping: AtomicBool,
}

struct AssignedSpiBinding {
    irq: HostIrqId,
    shared: Arc<AssignedSpiShared>,
    accepting: AtomicBool,
    delivery: AtomicU8,
    host_token: AtomicUsize,
}

impl AssignedSpiBinding {
    /// Publishes one acknowledged activation without VM lookup or locking.
    fn publish_from_irq(&self, token: usize) -> bool {
        if !self.accepting.load(Ordering::Acquire) {
            deactivate_host_irq(token);
            return true;
        }
        let mut observed = self.delivery.load(Ordering::Acquire);
        loop {
            match observed {
                // For a GICv3 HW-backed LR, normal guest EOI deactivates the
                // physical interrupt in hardware and does not call the
                // backend DIR hook. A subsequent host acknowledgement is the
                // architectural proof that the previous activation retired,
                // so it may replace this delivery marker.
                DELIVERY_IDLE | DELIVERY_ACTIVE => {
                    match self.delivery.compare_exchange_weak(
                        observed,
                        DELIVERY_RESERVING,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    ) {
                        Ok(_) => break,
                        Err(current) => observed = current,
                    }
                }
                DELIVERY_RESERVING | DELIVERY_INGRESS => {
                    // The source cannot legally be acknowledged again before
                    // the already published activation reaches the VGIC.
                    deactivate_host_irq(token);
                    return true;
                }
                _ => unreachable!("assigned SPI delivery state is out of range"),
            }
        }
        if !self.accepting.load(Ordering::Acquire) {
            self.delivery.store(DELIVERY_IDLE, Ordering::Release);
            deactivate_host_irq(token);
            return true;
        }

        self.host_token.store(token, Ordering::Release);
        self.delivery.store(DELIVERY_INGRESS, Ordering::Release);
        self.shared.notify.notify_irq();
        true
    }

    fn drain_ingress(&self) {
        if self
            .delivery
            .compare_exchange(
                DELIVERY_INGRESS,
                DELIVERY_ACTIVE,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return;
        }
        let token = self.host_token.swap(NO_HOST_TOKEN, Ordering::AcqRel);
        if let Err(error) = self.shared.controller.forward_physical_spi(self.irq) {
            self.delivery.store(DELIVERY_IDLE, Ordering::Release);
            if token != NO_HOST_TOKEN {
                deactivate_host_irq(token);
            }
            warn!(
                "failed to forward assigned physical SPI {} into the VGIC: {error}",
                self.irq.value()
            );
        }
    }

    fn complete(&self) -> bool {
        self.delivery
            .compare_exchange(
                DELIVERY_ACTIVE,
                DELIVERY_IDLE,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn release_unforwarded_activation(&self) {
        loop {
            match self.delivery.load(Ordering::Acquire) {
                DELIVERY_RESERVING => spin_loop(),
                DELIVERY_INGRESS => {
                    if self
                        .delivery
                        .compare_exchange(
                            DELIVERY_INGRESS,
                            DELIVERY_IDLE,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_err()
                    {
                        continue;
                    }
                    let token = self.host_token.swap(NO_HOST_TOKEN, Ordering::AcqRel);
                    if token != NO_HOST_TOKEN {
                        deactivate_host_irq(token);
                    }
                    return;
                }
                DELIVERY_IDLE | DELIVERY_ACTIVE => return,
                _ => unreachable!("assigned SPI delivery state is out of range"),
            }
        }
    }
}

struct AssignedSpiRouteSlot {
    binding: AtomicPtr<AssignedSpiBinding>,
    readers: AtomicUsize,
}

impl AssignedSpiRouteSlot {
    const fn new() -> Self {
        Self {
            binding: AtomicPtr::new(ptr::null_mut()),
            readers: AtomicUsize::new(0),
        }
    }

    fn with_binding(&self, operation: impl FnOnce(&AssignedSpiBinding) -> bool) -> bool {
        self.readers.fetch_add(1, Ordering::Acquire);
        let binding = self.binding.load(Ordering::Acquire);
        let result = if binding.is_null() {
            false
        } else {
            // SAFETY: removal clears the published pointer and waits for this
            // reader count before releasing the route-owned Arc.
            operation(unsafe { &*binding })
        };
        self.readers.fetch_sub(1, Ordering::Release);
        result
    }
}

struct AssignedSpiRouteRegistration {
    intid: usize,
    binding: usize,
}

impl AssignedSpiRouteRegistration {
    fn install(binding: &Arc<AssignedSpiBinding>) -> Result<Self, GicV3BackendError> {
        let intid = binding.irq.value();
        let Some(route) = ASSIGNED_SPI_ROUTES.get(intid) else {
            return Err(GicV3BackendError::new(
                "register assigned physical SPI route",
                alloc::format!("host INTID {intid} is outside the assignable GIC range"),
            ));
        };
        let raw = Arc::into_raw(binding.clone()) as *mut AssignedSpiBinding;
        if route
            .binding
            .compare_exchange(ptr::null_mut(), raw, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            // SAFETY: the compare-exchange did not publish this strong
            // reference, so this call consumes exactly the reference above.
            drop(unsafe { Arc::from_raw(raw) });
            return Err(GicV3BackendError::new(
                "register assigned physical SPI route",
                alloc::format!("host INTID {intid} is already assigned to another VM"),
            ));
        }
        Ok(Self {
            intid,
            binding: raw as usize,
        })
    }
}

impl Drop for AssignedSpiRouteRegistration {
    fn drop(&mut self) {
        let route = &ASSIGNED_SPI_ROUTES[self.intid];
        let expected = self.binding as *mut AssignedSpiBinding;
        if route
            .binding
            .compare_exchange(
                expected,
                ptr::null_mut(),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            warn!(
                "assigned physical SPI route {} changed before its owner released it",
                self.intid
            );
            return;
        }
        while route.readers.load(Ordering::Acquire) != 0 {
            spin_loop();
        }
        // SAFETY: installation transferred one strong reference to this
        // registration. The pointer is unpublished and all readers exited.
        drop(unsafe { Arc::from_raw(expected) });
    }
}

pub(super) fn route_acknowledged_host_irq(token: usize) -> Result<(), GicV3BackendError> {
    let intid = host_irq_intid(token) as usize;
    let published = ASSIGNED_SPI_ROUTES
        .get(intid)
        .is_some_and(|route| route.with_binding(|binding| binding.publish_from_irq(token)));
    if !published {
        dispatch_acknowledged_host_irq(token);
    }
    Ok(())
}

pub(super) fn complete_assigned_spi(irq: PhysicalIrqId) -> bool {
    usize::try_from(irq.raw())
        .ok()
        .and_then(|intid| ASSIGNED_SPI_ROUTES.get(intid))
        .is_some_and(|route| route.with_binding(AssignedSpiBinding::complete))
}
