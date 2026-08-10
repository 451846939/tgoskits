#![no_std]
#![no_main]
#![doc = include_str!("../../README.md")]

extern crate alloc;

use alloc::{borrow::ToOwned, vec::Vec};

use ax_std as _;

mod native_aicp;

#[cfg(feature = "nixos")]
pub const CMDLINE: &[&str] = &["/init"];

#[cfg(not(feature = "nixos"))]
pub const CMDLINE: &[&str] = &["/bin/sh", "-c", include_str!("init.sh")];

#[cfg(feature = "nixos")]
const ENVIRON: &[&str] = &["container=starryos"];

#[cfg(not(feature = "nixos"))]
const ENVIRON: &[&str] = &[];

#[unsafe(no_mangle)]
extern "C" fn main() {
    ax_log::ax_println!(
        "AICP_STARRY_MAIN_ENTER native={} target_os_none={}",
        option_env!("AICP_STARRY_NATIVE").unwrap_or("unset"),
        cfg!(target_os = "none")
    );
    if native_aicp::maybe_run() {
        ax_log::ax_println!("AICP_STARRY_NATIVE_EXIT");
        ax_std::process::exit(0);
    }

    let args = CMDLINE
        .iter()
        .copied()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let envs = ENVIRON
        .iter()
        .copied()
        .map(str::to_owned)
        .collect::<Vec<_>>();

    starry_kernel::entry::init(&args, &envs);
}

#[cfg(feature = "nixos")]
const _: () = assert!(command_eq(CMDLINE, &["/init"]));

#[cfg(not(feature = "nixos"))]
const _: () = assert!(command_eq(
    CMDLINE,
    &["/bin/sh", "-c", include_str!("init.sh")]
));

const fn command_eq(left: &[&str], right: &[&str]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut index = 0;
    while index < left.len() {
        if !bytes_eq(left[index].as_bytes(), right[index].as_bytes()) {
            return false;
        }
        index += 1;
    }
    true
}

const fn bytes_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut index = 0;
    while index < left.len() {
        if left[index] != right[index] {
            return false;
        }
        index += 1;
    }
    true
}
