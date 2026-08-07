use ax_errno::{AxError, AxResult};
use ax_runtime::task::{
    MembarrierCommand, MembarrierError, MembarrierRegistration, TaskError, membarrier,
    register_current_membarrier,
};
use linux_raw_sys::general::membarrier_cmd;

/// Memory barrier commands
const MEMBARRIER_CMD_QUERY: i32 = membarrier_cmd::MEMBARRIER_CMD_QUERY as i32;
const MEMBARRIER_CMD_GLOBAL: i32 = membarrier_cmd::MEMBARRIER_CMD_GLOBAL as i32;
const MEMBARRIER_CMD_GLOBAL_EXPEDITED: i32 = membarrier_cmd::MEMBARRIER_CMD_GLOBAL_EXPEDITED as i32;
const MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED: i32 =
    membarrier_cmd::MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED as i32;
const MEMBARRIER_CMD_PRIVATE_EXPEDITED: i32 =
    membarrier_cmd::MEMBARRIER_CMD_PRIVATE_EXPEDITED as i32;
const MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED: i32 =
    membarrier_cmd::MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED as i32;

/// Supported command flags for query
const SUPPORTED_COMMANDS: i32 = MEMBARRIER_CMD_GLOBAL
    | MEMBARRIER_CMD_GLOBAL_EXPEDITED
    | MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED
    | MEMBARRIER_CMD_PRIVATE_EXPEDITED
    | MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED;

fn map_membarrier_error(error: MembarrierError) -> AxError {
    match error {
        MembarrierError::NotRegistered => AxError::OperationNotPermitted,
        MembarrierError::Task(TaskError::InvalidConfiguration) => AxError::InvalidInput,
        MembarrierError::Task(TaskError::UnsafeContext) => AxError::OperationNotPermitted,
        MembarrierError::Task(_) => AxError::BadState,
    }
}

fn map_membarrier_registration_error(error: TaskError) -> AxError {
    map_membarrier_error(MembarrierError::Task(error))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MembarrierAction {
    Query,
    Global,
    RegisterGlobalExpedited,
    GlobalExpedited,
    RegisterPrivateExpedited,
    PrivateExpedited,
}

fn decode_membarrier_action(cmd: i32, flags: u32) -> AxResult<MembarrierAction> {
    if flags != 0 {
        return Err(AxError::InvalidInput);
    }
    match cmd {
        MEMBARRIER_CMD_QUERY => Ok(MembarrierAction::Query),
        MEMBARRIER_CMD_GLOBAL => Ok(MembarrierAction::Global),
        MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED => Ok(MembarrierAction::RegisterGlobalExpedited),
        MEMBARRIER_CMD_GLOBAL_EXPEDITED => Ok(MembarrierAction::GlobalExpedited),
        MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED => Ok(MembarrierAction::RegisterPrivateExpedited),
        MEMBARRIER_CMD_PRIVATE_EXPEDITED => Ok(MembarrierAction::PrivateExpedited),
        _ => Err(AxError::InvalidInput),
    }
}

pub fn sys_membarrier(
    _current: &crate::task::UserTaskRef,
    cmd: i32,
    flags: u32,
    _cpu_id: i32,
) -> AxResult<isize> {
    match decode_membarrier_action(cmd, flags)? {
        MembarrierAction::Query => Ok(SUPPORTED_COMMANDS as isize),
        MembarrierAction::Global => {
            membarrier(MembarrierCommand::Global).map_err(map_membarrier_error)?;
            Ok(0)
        }
        MembarrierAction::RegisterGlobalExpedited => {
            register_current_membarrier(MembarrierRegistration::GlobalExpedited)
                .map_err(map_membarrier_registration_error)?;
            Ok(0)
        }
        MembarrierAction::GlobalExpedited => {
            membarrier(MembarrierCommand::GlobalExpedited).map_err(map_membarrier_error)?;
            Ok(0)
        }
        MembarrierAction::RegisterPrivateExpedited => {
            register_current_membarrier(MembarrierRegistration::PrivateExpedited)
                .map_err(map_membarrier_registration_error)?;
            Ok(0)
        }
        MembarrierAction::PrivateExpedited => {
            membarrier(MembarrierCommand::PrivateExpedited).map_err(map_membarrier_error)?;
            Ok(0)
        }
    }
}

#[cfg(axtest)]
pub(crate) fn membarrier_query_and_global_rules_hold_for_test() -> bool {
    decode_membarrier_action(MEMBARRIER_CMD_QUERY, 0) == Ok(MembarrierAction::Query)
        && decode_membarrier_action(MEMBARRIER_CMD_QUERY, 1) == Err(AxError::InvalidInput)
        && decode_membarrier_action(-1, 0) == Err(AxError::InvalidInput)
        && decode_membarrier_action(MEMBARRIER_CMD_GLOBAL, 0) == Ok(MembarrierAction::Global)
}
