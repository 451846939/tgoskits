//! Operating-system capability boundary owned by the scheduler runtime.
//!
//! Runtime resources, clock-domain values, and provider operations are split
//! by owned invariant while retaining one trait-FFI table at the OS boundary.
mod capability;
mod clock;
mod interface;

pub use capability::*;
pub use clock::*;
pub use interface::*;
