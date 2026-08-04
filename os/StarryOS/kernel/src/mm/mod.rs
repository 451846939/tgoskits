//! User address space management and user-space memory access.

mod access;
mod aspace;
mod io;
mod loader;
mod stats;

pub use starry_mm::ProcessVmStat;

pub use self::{access::*, aspace::*, io::*, loader::*, stats::*};
#[cfg(axtest)]
pub(crate) use self::{
    aspace::cow_file_max_read_len_boundary_rules_hold_for_test,
    aspace::fault_accounting_failure_rolls_back_for_test,
};
