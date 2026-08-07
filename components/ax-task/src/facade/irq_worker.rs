use super::*;

pub(super) struct IrqWorkerWaiter {
    registration: IrqWaitRegistration,
    park: WaitQueue,
}

impl IrqWorkerWaiter {
    pub(super) fn new(wake_owner: ThreadWakeHandle) -> Self {
        Self {
            registration: IrqWaitRegistration::new(wake_owner),
            park: WaitQueue::new(),
        }
    }

    pub(super) fn wait(&self, event: &IrqWaitCell) -> Result<(), TaskError> {
        match event.register(&self.registration) {
            IrqRegisterResult::Occupied => Err(TaskError::InvalidConfiguration),
            IrqRegisterResult::ConsumedPending => Ok(()),
            IrqRegisterResult::Registered(token)
            | IrqRegisterResult::NotificationInFlight(token) => {
                let wait = self.park.try_wait_until(|| !token.is_attached());
                quiesce_irq_wait(token)?;
                wait
            }
        }
    }
}
