use std::{
    os::arceos::modules::{ax_hal, ax_task},
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

const NUM_TASKS: usize = 5;
const MIN_SLEEP_ADVANCE: Duration = Duration::from_millis(40);
static FINISHED_TASKS: AtomicUsize = AtomicUsize::new(0);

pub fn run() -> crate::TestResult {
    test_external_deadline();
    FINISHED_TASKS.store(0, Ordering::Release);
    let now = Instant::now();
    thread::sleep(Duration::from_millis(100));
    assert!(now.elapsed() >= MIN_SLEEP_ADVANCE);

    for i in 0..NUM_TASKS {
        thread::spawn(move || {
            let delay = Duration::from_millis(((i + 1) * 50) as u64);
            for _ in 0..2 {
                let now = Instant::now();
                thread::sleep(delay);
                assert!(now.elapsed() >= MIN_SLEEP_ADVANCE.min(delay / 2));
            }
            FINISHED_TASKS.fetch_add(1, Ordering::Release);
        });
    }

    while FINISHED_TASKS.load(Ordering::Acquire) < NUM_TASKS {
        thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

fn test_external_deadline() {
    const NO_DEADLINE: u64 = u64::MAX;
    let external_deadline = Arc::new(AtomicU64::new(NO_DEADLINE));
    let deadline_for_irq = Arc::clone(&external_deadline);
    ax_task::register_timer_irq_callback(move |now| {
        let deadline = deadline_for_irq.load(Ordering::Acquire);
        if deadline != NO_DEADLINE && now.as_nanos() >= deadline as u128 {
            deadline_for_irq.store(NO_DEADLINE, Ordering::Release);
        }
    });
    let published_deadline = Arc::clone(&external_deadline);
    ax_task::register_timer_deadline_source(move || {
        let deadline = published_deadline.load(Ordering::Acquire);
        (deadline != NO_DEADLINE).then_some(deadline)
    });

    let deadline = ax_hal::time::monotonic_time() + Duration::from_millis(10);
    let deadline_nanos = deadline.as_nanos().min(u64::MAX as u128) as u64;
    external_deadline.store(deadline_nanos, Ordering::Release);
    assert!(
        ax_task::next_timer_deadline_nanos().is_some_and(|selected| selected <= deadline_nanos)
    );
    ax_task::request_timer_deadline_nanos(deadline_nanos);
    while external_deadline.load(Ordering::Acquire) != NO_DEADLINE {
        thread::yield_now();
    }
}
