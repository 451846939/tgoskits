//! Loom models for the single-owner ax-task PI mutex core.

use loom::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
};

const OWNER_ONE: usize = 2;
const OWNER_TWO: usize = 4;
const HAS_WAITERS: usize = 1;

#[derive(Debug)]
struct CoreState {
    waiter_two: bool,
    waiter_three: bool,
    selected: usize,
    granted: usize,
    donation_owner: usize,
}

impl CoreState {
    fn owned_by_one() -> Self {
        Self {
            waiter_two: false,
            waiter_three: false,
            selected: 0,
            granted: 0,
            donation_owner: 0,
        }
    }
}

#[test]
fn owner_waiters_bit_closes_fast_unlock_registration_race() {
    loom::model(|| {
        let owner = Arc::new(AtomicUsize::new(OWNER_ONE));
        let core = Arc::new(Mutex::new(CoreState::owned_by_one()));

        let waiter = {
            let owner = Arc::clone(&owner);
            let core = Arc::clone(&core);
            thread::spawn(move || {
                let mut core = core.lock().unwrap();
                let snapshot = owner.load(Ordering::Acquire);
                if snapshot == 0
                    && owner
                        .compare_exchange(0, OWNER_TWO, Ordering::Acquire, Ordering::Relaxed)
                        .is_ok()
                {
                    return;
                }
                let snapshot = owner.fetch_or(HAS_WAITERS, Ordering::AcqRel);
                if snapshot == 0 {
                    owner.store(OWNER_TWO, Ordering::Release);
                } else {
                    core.waiter_two = true;
                    core.donation_owner = OWNER_ONE;
                }
            })
        };
        let unlock = {
            let owner = Arc::clone(&owner);
            let core = Arc::clone(&core);
            thread::spawn(move || {
                if owner
                    .compare_exchange(OWNER_ONE, 0, Ordering::Release, Ordering::Relaxed)
                    .is_ok()
                {
                    return;
                }
                let mut core = core.lock().unwrap();
                assert!(core.waiter_two);
                core.selected = OWNER_TWO;
                core.donation_owner = 0;
                owner.store(HAS_WAITERS, Ordering::Release);
            })
        };

        waiter.join().unwrap();
        unlock.join().unwrap();

        let core = core.lock().unwrap();
        match owner.load(Ordering::Acquire) {
            OWNER_TWO => assert!(!core.waiter_two),
            HAS_WAITERS => {
                assert!(core.waiter_two);
                assert_eq!(core.selected, OWNER_TWO);
                assert_eq!(core.donation_owner, 0);
            }
            owner => panic!("registration/unlock race lost owner state {owner}"),
        }
    });
}

#[test]
fn selection_deboost_and_ownerless_state_publish_before_wake() {
    loom::model(|| {
        let owner = Arc::new(AtomicUsize::new(OWNER_ONE | HAS_WAITERS));
        let core = Arc::new(Mutex::new(CoreState {
            waiter_two: true,
            waiter_three: false,
            selected: 0,
            granted: 0,
            donation_owner: OWNER_ONE,
        }));
        let wake = Arc::new(AtomicBool::new(false));

        let unlock = {
            let owner = Arc::clone(&owner);
            let core = Arc::clone(&core);
            let wake = Arc::clone(&wake);
            thread::spawn(move || {
                let mut core = core.lock().unwrap();
                core.selected = OWNER_TWO;
                core.donation_owner = 0;
                owner.store(HAS_WAITERS, Ordering::Release);
                drop(core);
                wake.store(true, Ordering::Release);
            })
        };
        let waiter = {
            let owner = Arc::clone(&owner);
            let core = Arc::clone(&core);
            let wake = Arc::clone(&wake);
            thread::spawn(move || {
                while !wake.load(Ordering::Acquire) {
                    thread::yield_now();
                }
                let mut core = core.lock().unwrap();
                assert_eq!(owner.load(Ordering::Acquire), HAS_WAITERS);
                assert_eq!(core.selected, OWNER_TWO);
                assert_eq!(core.donation_owner, 0);
                core.waiter_two = false;
                core.selected = 0;
                core.granted = OWNER_TWO;
                owner.store(OWNER_TWO, Ordering::Release);
            })
        };

        unlock.join().unwrap();
        waiter.join().unwrap();
        let core = core.lock().unwrap();
        assert_eq!(owner.load(Ordering::Acquire), OWNER_TWO);
        assert_eq!(core.granted, OWNER_TWO);
        assert!(!core.waiter_two);
    });
}

#[test]
fn ownerless_newcomer_cannot_steal_the_selected_claim() {
    loom::model(|| {
        let owner = Arc::new(AtomicUsize::new(HAS_WAITERS));
        let core = Arc::new(Mutex::new(CoreState {
            waiter_two: true,
            waiter_three: false,
            selected: OWNER_TWO,
            granted: 0,
            donation_owner: 0,
        }));

        let claimant = {
            let owner = Arc::clone(&owner);
            let core = Arc::clone(&core);
            thread::spawn(move || {
                let mut core = core.lock().unwrap();
                assert_eq!(owner.load(Ordering::Acquire), HAS_WAITERS);
                assert_eq!(core.selected, OWNER_TWO);
                core.waiter_two = false;
                core.selected = 0;
                core.granted = OWNER_TWO;
                let has_waiters = core.waiter_three;
                owner.store(
                    OWNER_TWO | if has_waiters { HAS_WAITERS } else { 0 },
                    Ordering::Release,
                );
                if has_waiters {
                    core.donation_owner = OWNER_TWO;
                }
            })
        };
        let newcomer = {
            let owner = Arc::clone(&owner);
            let core = Arc::clone(&core);
            thread::spawn(move || {
                let mut core = core.lock().unwrap();
                assert_ne!(owner.load(Ordering::Acquire), 0);
                owner.fetch_or(HAS_WAITERS, Ordering::AcqRel);
                core.waiter_three = true;
                core.donation_owner = OWNER_TWO;
            })
        };

        claimant.join().unwrap();
        newcomer.join().unwrap();
        let core = core.lock().unwrap();
        assert_eq!(owner.load(Ordering::Acquire), OWNER_TWO | HAS_WAITERS);
        assert_eq!(core.granted, OWNER_TWO);
        assert!(core.waiter_three);
        assert_eq!(core.donation_owner, OWNER_TWO);
    });
}

#[test]
fn cancellation_and_release_have_one_serialized_winner() {
    loom::model(|| {
        let owner = Arc::new(AtomicUsize::new(OWNER_ONE | HAS_WAITERS));
        let core = Arc::new(Mutex::new(CoreState {
            waiter_two: true,
            waiter_three: false,
            selected: 0,
            granted: 0,
            donation_owner: OWNER_ONE,
        }));
        let wake = Arc::new(AtomicBool::new(false));

        let cancel = {
            let owner = Arc::clone(&owner);
            let core = Arc::clone(&core);
            thread::spawn(move || {
                let mut core = core.lock().unwrap();
                if core.selected == 0 {
                    core.waiter_two = false;
                    core.donation_owner = 0;
                    owner.store(OWNER_ONE, Ordering::Release);
                }
            })
        };
        let release = {
            let owner = Arc::clone(&owner);
            let core = Arc::clone(&core);
            let wake = Arc::clone(&wake);
            thread::spawn(move || {
                let mut core = core.lock().unwrap();
                if !core.waiter_two {
                    return;
                }
                core.selected = OWNER_TWO;
                core.donation_owner = 0;
                owner.store(HAS_WAITERS, Ordering::Release);
                drop(core);
                wake.store(true, Ordering::Release);
            })
        };

        cancel.join().unwrap();
        release.join().unwrap();
        let core = core.lock().unwrap();
        if core.waiter_two {
            assert_eq!(core.selected, OWNER_TWO);
            assert_eq!(owner.load(Ordering::Acquire), HAS_WAITERS);
            assert!(wake.load(Ordering::Acquire));
        } else {
            assert_eq!(core.selected, 0);
            assert_eq!(owner.load(Ordering::Acquire), OWNER_ONE);
            assert!(!wake.load(Ordering::Acquire));
        }
        assert_eq!(core.donation_owner, 0);
    });
}
