use super::*;

/// Enters the scheduler-owned PI mutex slow path.
pub fn pi_mutex_lock_slow<'lock>(
    lock: PiMutexRef<'lock>,
    sequence: u64,
) -> Result<PiMutexLockResult<'lock>, TaskError> {
    let waiter = current_thread_id()?;
    runtime_task_system()?.pi_mutex_lock_slow(lock, waiter, sequence)
}

/// Blocks the calling waiter until it is selected to claim or granted.
pub fn pi_block_current(token: &PiWaitToken<'_>) -> Result<(), TaskError> {
    if token.is_selected() || token.is_granted() {
        return Ok(());
    }
    let system = runtime_task_system()?;
    if runtime_current_cpu()?.current() != Some(token.thread_id()) {
        return Err(TaskError::InvalidPiState);
    }
    loop {
        {
            let mut irq = RuntimeIrqGuard::enter();
            let now_ns = task_runtime::monotonic_ns();
            let mut cpu = runtime_current_cpu_mut(&mut irq)?;
            system.drain_policy_updates(cpu.as_mut(), now_ns)?;
        }
        if token.is_selected() || token.is_granted() {
            return Ok(());
        }
        let mut ticket = {
            let permit = acquire_blocking_permit()?;
            match prepare_current_park(&permit)? {
                ParkPrepare::Notified => continue,
                ParkPrepare::Prepared(ticket) => ticket,
            }
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

/// Cancels a PI wait token after a handoff-before-block race.
pub fn pi_wait_cancel(token: PiWaitToken<'_>) -> Result<(), TaskError> {
    runtime_task_system()?.pi_wait_cancel(token)
}

/// Publishes a contended PI mutex release and returns its wake target.
pub fn pi_mutex_release(lock: PiMutexRef<'_>) -> Result<ThreadWakeHandle, TaskError> {
    runtime_task_system()?.pi_mutex_release(lock, current_thread_id()?)
}

/// Claims the ownerless PI mutex handoff selected for this waiter.
pub fn pi_mutex_claim(token: &PiWaitToken<'_>) -> Result<(), TaskError> {
    if current_thread_id()? != token.thread_id() {
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
