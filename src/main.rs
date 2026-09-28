//! stormnic-ixgbe: an `EFI_SIMPLE_NETWORK_PROTOCOL` driver for Intel 10G, in Rust.
//!
//! An EFI boot-service driver (see build.rs). The entry point installs an
//! `EFI_DRIVER_BINDING_PROTOCOL` on the image handle and returns; the
//! firmware's `ConnectController` (stormbootx runs it after loading every
//! driver on its media) then calls Supported/Start for each controller.
//! Bring-up and the SNP are the work plan in CLAUDE.md.
#![no_main]
#![no_std]

extern crate alloc;

mod binding;
mod ids;
mod pci_io;

use uefi::prelude::*;

#[entry]
fn main() -> Status {
    if uefi::helpers::init().is_err() {
        return Status::LOAD_ERROR;
    }
    let version = env!("CARGO_PKG_VERSION");
    match uefi::driver::install(binding::IxgbeDriver::new(), None) {
        Ok(()) => {
            uefi::println!(
                "stormnic-ixgbe {version}: driver binding installed ({} Intel 10G device IDs)",
                ids::SUPPORTED.len()
            );
            Status::SUCCESS
        }
        Err(e) => {
            uefi::println!("stormnic-ixgbe {version}: driver binding not installed: {:?}", e.status());
            e.status()
        }
    }
}
