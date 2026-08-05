use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

use super::*;

#[test]
fn consumes_an_irq_that_arrived_before_registration_without_self_wake() {
    let cell = IrqWaitCell::new();
    let registration = TestRegistration::new();

    assert_eq!(cell.notify(), IrqNotifyResult::Pending);
    assert!(matches!(
        cell.register(registration.registration()),
        IrqRegisterResult::ConsumedPending
    ));
    assert_eq!(
        registration.wake_count(),
        0,
        "the running consumer must not publish a stale scheduler wake for an event it consumed \
         synchronously"
    );
    let token = expect_registered(cell.register(registration.registration()));
    assert_eq!(cell.notify(), IrqNotifyResult::Notified);
    assert_eq!(registration.wake_count(), 1);
    let drain = token.detach();
    assert!(drain.is_quiescent());
    drain.try_finish().unwrap();
}

#[test]
fn irq_wakes_the_single_registered_thread() {
    let cell = IrqWaitCell::new();
    let registration = TestRegistration::new();

    let token = expect_registered(cell.register(registration.registration()));
    assert_eq!(cell.notify(), IrqNotifyResult::Notified);
    assert_eq!(registration.wake_count(), 1);
    token.detach().try_finish().unwrap();
    assert_eq!(cell.notify(), IrqNotifyResult::Pending);
}

#[test]
fn rejects_a_second_waiter_without_scanning() {
    let cell = IrqWaitCell::new();
    let first = TestRegistration::new();
    let second = TestRegistration::new();

    let token = expect_registered(cell.register(first.registration()));
    assert!(matches!(
        cell.register(second.registration()),
        IrqRegisterResult::Occupied
    ));
    token.detach().try_finish().unwrap();
}

#[test]
fn detaching_a_waiter_enters_a_distinct_drain_lifetime() {
    let cell = IrqWaitCell::new();
    let registration = TestRegistration::new();

    let token = expect_registered(cell.register(registration.registration()));
    let drain = token.detach();

    assert!(drain.is_quiescent());
    drain.try_finish().unwrap();
}

#[test]
fn cell_owns_a_published_node_after_its_token_is_abandoned() {
    let cell = IrqWaitCell::new();
    let registration = TestRegistration::new();
    let token = expect_registered(cell.register(registration.registration()));

    core::mem::forget(token);
    drop(registration);

    assert_eq!(
        cell.notify(),
        IrqNotifyResult::Notified,
        "the cell must retain an owning reference until it revokes the published node",
    );
}

#[test]
fn cell_teardown_revokes_a_published_node_before_reuse() {
    let registration = TestRegistration::new();
    let cell = IrqWaitCell::new();
    let token = expect_registered(cell.register(registration.registration()));

    drop(token);
    drop(cell);

    let next_cell = IrqWaitCell::new();
    let next = expect_registered(next_cell.register(registration.registration()));
    next.detach().try_finish().unwrap();
}

#[test]
fn detached_registration_is_not_quiescent_until_irq_wake_returns() {
    struct BlockingWake {
        entered: AtomicUsize,
        release: AtomicUsize,
        completed: AtomicUsize,
    }

    let cell = IrqWaitCell::new();
    let state = Arc::new(BlockingWake {
        entered: AtomicUsize::new(0),
        release: AtomicUsize::new(0),
        completed: AtomicUsize::new(0),
    });
    let wake_state = Arc::clone(&state);
    let wake = IrqWakeHandle::from_fn(move || {
        wake_state.entered.store(1, Ordering::Release);
        while wake_state.release.load(Ordering::Acquire) == 0 {
            core::hint::spin_loop();
        }
        wake_state.completed.store(1, Ordering::Release);
    });
    let registration = IrqWaitRegistration::new_test(wake);
    let token = expect_registered(cell.register(&registration));

    let drain = std::thread::scope(|scope| {
        let notifier = scope.spawn(|| cell.notify());
        while state.entered.load(Ordering::Acquire) == 0 {
            std::thread::yield_now();
        }

        assert!(!token.is_attached());
        let drain = token.detach();
        let quiescent_while_wake_uses_payload = drain.is_quiescent();
        drop(registration);

        state.release.store(1, Ordering::Release);
        assert_eq!(notifier.join().unwrap(), IrqNotifyResult::Notified);
        assert!(
            !quiescent_while_wake_uses_payload,
            "detachment must not authorize reclamation while the IRQ wake still reads its payload"
        );
        drain
    });
    assert_eq!(state.completed.load(Ordering::Acquire), 1);
    drain.try_finish().unwrap();
}

#[test]
fn second_irq_during_registration_wake_remains_pending() {
    struct BlockingWake {
        entered: AtomicUsize,
        release: AtomicUsize,
    }

    let cell = IrqWaitCell::new();
    let state = Arc::new(BlockingWake {
        entered: AtomicUsize::new(0),
        release: AtomicUsize::new(0),
    });
    let wake_state = Arc::clone(&state);
    let wake = IrqWakeHandle::from_fn(move || {
        wake_state.entered.store(1, Ordering::Release);
        while wake_state.release.load(Ordering::Acquire) == 0 {
            core::hint::spin_loop();
        }
    });
    let registration = IrqWaitRegistration::new_test(wake);
    cell.pause_after_register_publish
        .store(true, Ordering::Release);

    let (pending_survived, token) = std::thread::scope(|scope| {
        let register = scope.spawn(|| cell.register(&registration));
        while !cell.register_published.load(Ordering::Acquire) {
            std::thread::yield_now();
        }

        let first_irq = scope.spawn(|| cell.notify());
        while state.entered.load(Ordering::Acquire) == 0 {
            std::thread::yield_now();
        }
        assert_eq!(cell.notify(), IrqNotifyResult::Pending);

        cell.pause_after_register_publish
            .store(false, Ordering::Release);
        let token = match register.join().unwrap() {
            IrqRegisterResult::NotificationInFlight(token) => token,
            other => panic!("expected the first IRQ to own the registration, got {other:?}"),
        };
        let pending_survived = cell.is_pending();

        state.release.store(1, Ordering::Release);
        assert_eq!(first_irq.join().unwrap(), IrqNotifyResult::Notified);
        (pending_survived, token)
    });

    token.detach().try_finish().unwrap();
    let consumed_by_next_registration = match cell.register(&registration) {
        IrqRegisterResult::ConsumedPending => true,
        IrqRegisterResult::Registered(token) => {
            token.detach().try_finish().unwrap();
            false
        }
        IrqRegisterResult::NotificationInFlight(token) => {
            token.detach().try_finish().unwrap();
            false
        }
        IrqRegisterResult::Occupied => false,
    };
    assert!(
        pending_survived,
        "registration must not clear an IRQ published while another wake is in flight"
    );
    assert!(
        consumed_by_next_registration,
        "the next registration must consume the second IRQ"
    );
}

#[test]
fn old_detach_cannot_remove_a_rearmed_registration_with_the_same_node_address() {
    let cell = IrqWaitCell::new();
    let registration = TestRegistration::new();
    let old = expect_registered(cell.register(registration.registration()));
    cell.pause_after_detach_generation_check
        .store(true, Ordering::Release);

    std::thread::scope(|scope| {
        let detach = scope.spawn(|| old.detach());
        while !cell.detach_generation_checked.load(Ordering::Acquire) {
            std::thread::yield_now();
        }

        assert_eq!(cell.notify(), IrqNotifyResult::Notified);
        assert!(
            matches!(
                cell.register(registration.registration()),
                IrqRegisterResult::Occupied
            ),
            "a completed IRQ generation must remain unavailable until its drain token finishes"
        );

        cell.pause_after_detach_generation_check
            .store(false, Ordering::Release);
        let old_drain = detach
            .join()
            .expect("old detach must not corrupt the new generation");
        old_drain.try_finish().unwrap();

        let new = expect_registered(cell.register(registration.registration()));
        assert!(new.is_attached());
        new.detach().try_finish().unwrap();
    });
}

fn expect_registered(result: IrqRegisterResult<'_>) -> IrqWaitToken<'_> {
    match result {
        IrqRegisterResult::Registered(token) => token,
        other => panic!("expected a registered IRQ waiter, got {other:?}"),
    }
}

struct TestRegistration {
    registration: IrqWaitRegistration,
    wakes: Arc<AtomicUsize>,
}

impl TestRegistration {
    fn new() -> Self {
        let wakes = Arc::new(AtomicUsize::new(0));
        let wake_counter = Arc::clone(&wakes);
        let wake = IrqWakeHandle::from_fn(move || {
            wake_counter.fetch_add(1, Ordering::Relaxed);
        });
        Self {
            registration: IrqWaitRegistration::new_test(wake),
            wakes,
        }
    }

    fn registration(&self) -> &IrqWaitRegistration {
        &self.registration
    }

    fn wake_count(&self) -> usize {
        self.wakes.load(Ordering::Relaxed)
    }
}
