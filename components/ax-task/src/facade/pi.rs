use super::*;

pub(super) enum PiParkAttempt {
    Complete,
    Retry,
    Prepared(crate::ParkTicket),
}

/// Enters the scheduler-owned PI mutex slow path.
pub fn pi_mutex_lock_slow<'lock>(
    lock: PiMutexRef<'lock>,
    current: &CurrentThreadToken,
    sequence: u64,
) -> Result<PiMutexLockResult<'lock>, TaskError> {
    runtime_task_system()?.pi_mutex_lock_slow(lock, current.id(), sequence)
}

/// Blocks the calling waiter until it is selected to claim or granted.
pub fn pi_block_current(token: &PiWaitToken<'_>) -> Result<(), TaskError> {
    if token.is_selected() || token.is_granted() {
        return Ok(());
    }
    let system = runtime_task_system()?;
    loop {
        let mut ticket = match prepare_pi_park_attempt(system, token)? {
            PiParkAttempt::Complete => return Ok(()),
            PiParkAttempt::Retry => continue,
            PiParkAttempt::Prepared(ticket) => ticket,
        };
        if token.is_selected() || token.is_granted() {
            cancel_current_park(&mut ticket)?;
            return Ok(());
        }
        commit_current_park(&mut ticket)?;
        if token.is_selected() || token.is_granted() {
            return Ok(());
        }
    }
}

pub(super) fn prepare_pi_park_attempt(
    system: &TaskSystem,
    token: &PiWaitToken<'_>,
) -> Result<PiParkAttempt, TaskError> {
    let _permit = acquire_blocking_permit()?;
    let mut irq = RuntimeIrqGuard::enter();
    let now_ns = task_runtime::monotonic_ns();
    let mut cpu = runtime_current_cpu_mut(&mut irq)?;
    if cpu.current() != Some(token.thread_id()) {
        return Err(TaskError::InvalidPiState);
    }
    system.drain_policy_updates(cpu.as_mut(), now_ns)?;
    if token.is_selected() || token.is_granted() {
        return Ok(PiParkAttempt::Complete);
    }
    match system.prepare_park(cpu.as_mut())? {
        ParkPrepare::Notified => Ok(PiParkAttempt::Retry),
        ParkPrepare::Prepared(ticket) => Ok(PiParkAttempt::Prepared(ticket)),
    }
}

/// Cancels a PI wait token after a handoff-before-block race.
pub fn pi_wait_cancel(token: PiWaitToken<'_>) -> Result<(), TaskError> {
    runtime_task_system()?.pi_wait_cancel(token)
}

/// Publishes a contended PI mutex release and returns its wake target.
pub fn pi_mutex_release(
    lock: PiMutexRef<'_>,
    current: &CurrentThreadToken,
) -> Result<ThreadWakeHandle, TaskError> {
    runtime_task_system()?.pi_mutex_release(lock, current.id())
}

/// Claims the ownerless PI mutex handoff selected for this waiter.
pub fn pi_mutex_claim(
    token: &PiWaitToken<'_>,
    current: &CurrentThreadToken,
) -> Result<(), TaskError> {
    if current.id() != token.thread_id() {
        return Err(TaskError::InvalidPiState);
    }
    runtime_task_system()?.pi_mutex_claim(token)
}

/// Publishes a targeted task-context wake after PI metadata handoff.
pub fn pi_wake(wake: &ThreadWakeHandle) -> Result<(), TaskError> {
    match wake.wake_from_task() {
        WakeResult::Notified | WakeResult::AlreadyPending | WakeResult::Exited => Ok(()),
        WakeResult::Unavailable => Err(TaskError::NotInitialized),
    }
}
