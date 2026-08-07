//! Shared IPI transport for scheduler doorbells and generic callbacks.

#[cfg(all(feature = "irq", feature = "ipi"))]
pub(crate) unsafe fn run_on_cpu_sync(
    cpu: usize,
    f: unsafe fn(*mut ()),
    arg: *mut (),
) -> Result<(), ax_hal::irq::IrqError> {
    // SAFETY: the caller supplies the hard-call ABI and lifetime required by
    // ax-ipi; this adapter only forwards the typed platform hook.
    unsafe { ax_ipi::call_on_cpu(ax_hal::irq::CpuId(cpu), f, arg) }
}

#[cfg(any(feature = "ipi", feature = "wake-ipi", test))]
fn dispatch_shared_ipi(
    drain_callbacks: impl FnOnce(),
    scheduler_work_pending: impl FnOnce() -> bool,
    publish_scheduler_work: impl FnOnce(),
) {
    if scheduler_work_pending() {
        publish_scheduler_work();
    }
    drain_callbacks();
}

#[cfg(all(feature = "multitask", any(feature = "ipi", feature = "wake-ipi")))]
fn local_scheduler_work_pending() -> bool {
    let pending = crate::task::current_cpu_needs_resched()
        .expect("IPI delivery requires an online scheduler CPU");
    #[cfg(feature = "qperf-metrics")]
    if pending {
        crate::task::record_scheduler_ipi_consume();
    }
    pending
}

#[cfg(all(feature = "irq", feature = "ipi"))]
pub(crate) fn irq_handler(_ctx: ax_hal::irq::IrqContext) -> ax_hal::irq::IrqReturn {
    ax_ipi::claim_current_delivery();
    dispatch_shared_ipi(
        || {
            ax_ipi::drain_hard_calls()
                .unwrap_or_else(|error| panic!("failed to continue hard-call draining: {error:?}"));
            ax_ipi::legacy::drain_current_callbacks();
        },
        || {
            #[cfg(feature = "multitask")]
            {
                local_scheduler_work_pending()
            }
            #[cfg(not(feature = "multitask"))]
            {
                false
            }
        },
        || {
            #[cfg(feature = "multitask")]
            {
                let _self_serviced = crate::guard::publish_local_scheduler_work();
            }
        },
    );
    ax_hal::irq::IrqReturn::Handled
}

#[cfg(all(feature = "irq", feature = "wake-ipi", not(feature = "ipi")))]
pub(crate) fn irq_handler(_ctx: ax_hal::irq::IrqContext) -> ax_hal::irq::IrqReturn {
    ax_ipi::claim_current_delivery();
    dispatch_shared_ipi(
        || {},
        || {
            #[cfg(feature = "multitask")]
            {
                local_scheduler_work_pending()
            }
            #[cfg(not(feature = "multitask"))]
            {
                false
            }
        },
        || {
            #[cfg(feature = "multitask")]
            {
                let _self_serviced = crate::guard::publish_local_scheduler_work();
            }
        },
    );
    ax_hal::irq::IrqReturn::Handled
}

#[cfg(test)]
mod tests {
    use core::cell::{Cell, RefCell};

    #[test]
    fn shared_ipi_dispatch_consumes_scheduler_delivery_before_callback_drain() {
        let events = RefCell::new(alloc::vec::Vec::new());

        super::dispatch_shared_ipi(
            || events.borrow_mut().push("callbacks"),
            || {
                events.borrow_mut().push("consume");
                true
            },
            || events.borrow_mut().push("publish"),
        );

        assert_eq!(*events.borrow(), ["consume", "publish", "callbacks"]);
    }

    #[test]
    fn shared_ipi_callback_can_publish_a_fresh_scheduler_epoch() {
        let scheduler_epoch_claimed = Cell::new(true);

        super::dispatch_shared_ipi(
            || {
                assert!(
                    !scheduler_epoch_claimed.get(),
                    "the delivered scheduler epoch must be released at IPI entry"
                );
                scheduler_epoch_claimed.set(true);
            },
            || {
                scheduler_epoch_claimed.set(false);
                true
            },
            || {},
        );

        assert!(
            scheduler_epoch_claimed.get(),
            "a scheduler delivery published during callback drain must remain pending"
        );
    }

    #[test]
    fn unrelated_shared_ipi_only_checks_the_scheduler_doorbell() {
        let events = RefCell::new(alloc::vec::Vec::new());

        super::dispatch_shared_ipi(
            || events.borrow_mut().push("callbacks"),
            || {
                events.borrow_mut().push("consume");
                false
            },
            || events.borrow_mut().push("publish"),
        );

        assert_eq!(*events.borrow(), ["consume", "callbacks"]);
    }
}
