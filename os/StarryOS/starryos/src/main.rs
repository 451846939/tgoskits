#![cfg_attr(target_os = "none", no_std)]
#![cfg_attr(target_os = "none", no_main)]
#![doc = include_str!("../../README.md")]

extern crate alloc;

#[cfg(feature = "starry-userland")]
use alloc::{borrow::ToOwned, vec::Vec};

use ax_std as _;

mod native_aicp;

pub const CMDLINE: &[&str] = &["/bin/sh", "-c", include_str!("init.sh")];

#[cfg_attr(target_os = "none", unsafe(no_mangle))]
fn main() {
    ax_log::ax_println!(
        "AICP_STARRY_MAIN_ENTER native={} target_os_none={}",
        option_env!("AICP_STARRY_NATIVE").unwrap_or("unset"),
        cfg!(target_os = "none")
    );
    if native_aicp::maybe_run() {
        ax_log::ax_println!("AICP_STARRY_NATIVE_EXIT");
        ax_std::process::exit(0);
    }

    #[cfg(feature = "starry-userland")]
    {
        let args = CMDLINE
            .iter()
            .copied()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let envs = [];

        starry_kernel::entry::init(&args, &envs);
    }

    #[cfg(not(feature = "starry-userland"))]
    {
        ax_log::ax_println!("AICP_STARRY_NO_USERLAND feature=starry-userland-disabled");
        ax_std::process::exit(0);
    }
}
