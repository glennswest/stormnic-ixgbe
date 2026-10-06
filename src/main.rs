//! stormnic-ixgbe: an `EFI_SIMPLE_NETWORK_PROTOCOL` driver for Intel 10G, in Rust.
//!
//! An EFI boot-service driver (see build.rs). The entry point installs an
//! `EFI_DRIVER_BINDING_PROTOCOL` on the image handle (binding.rs) and returns; the
//! firmware's `ConnectController` (stormbootx runs it after loading every
//! driver on its media) then calls Supported/Start for each controller.
//! Start brings the NIC up and puts an `EFI_SIMPLE_NETWORK_PROTOCOL` on a
//! child handle; stormbootx opens it EXCLUSIVE and runs smoltcp on it.
#![no_main]
#![no_std]

extern crate alloc;

// First: its macros (say!, trace!, fail!, note!) are used by the modules below.
#[macro_use]
mod console;
mod binding;
mod decode;
mod hardware;
mod ids;
mod pci_io;
mod snp;
mod trace;

use uefi::prelude::*;

#[entry]
fn main() -> Status {
    if uefi::helpers::init().is_err() {
        return Status::LOAD_ERROR;
    }
    console::init();
    let version = env!("CARGO_PKG_VERSION");
    match binding::install() {
        Ok(()) => {
            trace!(
                "stormnic-ixgbe {version}: driver binding installed ({} Intel 10G device IDs)",
                ids::SUPPORTED.len()
            );
            Status::SUCCESS
        }
        Err(e) => {
            say!("stormnic-ixgbe {version}: driver binding not installed: {:?}", e.status());
            e.status()
        }
    }
}
