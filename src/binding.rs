//! `EFI_DRIVER_BINDING_PROTOCOL`: which controllers this driver takes.
//!
//! Supported looks at every PCI function the firmware offers. It reads the
//! vendor/device ID with a non-exclusive (GET_PROTOCOL) open, and only for one
//! of ours tries the BY_DRIVER open that Start will hold. A platform driver
//! that already owns the NIC holds that open, so ours fails and the platform's
//! driver wins: a medium carrying this driver is safe on a machine that has
//! its own.
//!
//! Start holds `EFI_PCI_IO_PROTOCOL` BY_DRIVER for the controller; Stop
//! releases it. Bring-up and the SNP child are later work (#2–#4).
//!
//! Every Intel network function Supported sees is logged, matched or not, so a
//! boot names the device ID the machine really has. Nothing else is: the
//! firmware calls Supported for every handle on every ConnectController.

use alloc::vec::Vec;
use uefi::boot::{self, OpenProtocolAttributes, OpenProtocolParams, ScopedProtocol};
use uefi::driver::Driver;
use uefi::proto::device_path::DevicePath;
use uefi::{println, Handle, Result, Status};

use crate::ids::{self, Nic};
use crate::pci_io::{Location, PciIo};

/// A controller this driver has started.
struct Bound {
    controller: Handle,
    location: Option<Location>,
    /// The BY_DRIVER open; dropping it closes the protocol.
    _pci: ScopedProtocol<PciIo>,
}

pub struct IxgbeDriver {
    bound: Vec<Bound>,
}

impl IxgbeDriver {
    pub const fn new() -> Self {
        IxgbeDriver { bound: Vec::new() }
    }
}

/// What config space says about a function.
struct Ident {
    vendor: u16,
    device: u16,
    class: u8,
}

fn ident(pci: &PciIo) -> Result<Ident> {
    let id = pci.config_read_u32(0x00)?;
    let class = pci.config_read_u32(0x08)?;
    Ok(Ident {
        vendor: id as u16,
        device: (id >> 16) as u16,
        class: (class >> 24) as u8,
    })
}

fn open(agent: Handle, controller: Handle, attrs: OpenProtocolAttributes) -> Result<ScopedProtocol<PciIo>> {
    let params = OpenProtocolParams { handle: controller, agent, controller: Some(controller) };
    // SAFETY: GET_PROTOCOL opens are dropped before Supported/Start return;
    // a BY_DRIVER open is tracked by the firmware, which calls Stop before
    // it removes the interface.
    unsafe { boot::open_protocol::<PciIo>(params, attrs) }
}

fn at(location: Option<Location>) -> impl core::fmt::Display {
    struct At(Option<Location>);
    impl core::fmt::Display for At {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            match self.0 {
                Some(l) => write!(f, "{l}"),
                None => f.write_str("(location unknown)"),
            }
        }
    }
    At(location)
}

impl IxgbeDriver {
    fn is_bound(&self, controller: Handle) -> bool {
        self.bound.iter().any(|b| b.controller == controller)
    }

    /// The controller's NIC entry, if it is one of ours, logging any Intel
    /// network function that is not.
    fn ours(&self, agent: Handle, controller: Handle) -> Result<(&'static Nic, Option<Location>)> {
        // No PciIo: not a PCI function; the common case, and silent.
        let pci = open(agent, controller, OpenProtocolAttributes::GetProtocol)?;
        let id = ident(&pci)?;
        if id.vendor != ids::INTEL || id.class != ids::CLASS_NETWORK {
            return Err(Status::UNSUPPORTED.into());
        }
        let location = pci.location().ok();
        match ids::lookup(id.vendor, id.device) {
            Some(nic) => Ok((nic, location)),
            None => {
                println!(
                    "stormnic-ixgbe: {} 8086:{:04x}: Intel network function, not in the 82599/X540/X552 list; not binding",
                    at(location),
                    id.device
                );
                Err(Status::UNSUPPORTED.into())
            }
        }
    }
}

impl Driver for IxgbeDriver {
    fn supported(&mut self, agent: Handle, controller: Handle, _remaining: Option<&DevicePath>) -> Result {
        if self.is_bound(controller) {
            return Err(Status::ALREADY_STARTED.into());
        }
        let (nic, location) = self.ours(agent, controller)?;
        match open(agent, controller, OpenProtocolAttributes::ByDriver) {
            Ok(_pci) => {
                println!(
                    "stormnic-ixgbe: {} 8086:{:04x} {}: Supported",
                    at(location),
                    nic.device,
                    nic.name
                );
                Ok(())
            }
            Err(e) => {
                println!(
                    "stormnic-ixgbe: {} 8086:{:04x} {}: already driven by another driver ({:?}); leaving it",
                    at(location),
                    nic.device,
                    nic.name,
                    e.status()
                );
                Err(e)
            }
        }
    }

    fn start(&mut self, agent: Handle, controller: Handle, _remaining: Option<&DevicePath>) -> Result {
        let (nic, location) = self.ours(agent, controller)?;
        let pci = match open(agent, controller, OpenProtocolAttributes::ByDriver) {
            Ok(pci) => pci,
            Err(e) => {
                println!(
                    "stormnic-ixgbe: {} 8086:{:04x}: Start could not open PciIo BY_DRIVER: {:?}",
                    at(location),
                    nic.device,
                    e.status()
                );
                return Err(e);
            }
        };
        self.bound.push(Bound { controller, location, _pci: pci });
        println!(
            "stormnic-ixgbe: {} 8086:{:04x} {}: Start: bound (scaffold: no SNP yet)",
            at(location),
            nic.device,
            nic.name
        );
        Ok(())
    }

    fn stop(&mut self, _agent: Handle, controller: Handle) -> Result {
        match self.bound.iter().position(|b| b.controller == controller) {
            Some(i) => {
                let b = self.bound.swap_remove(i);
                println!("stormnic-ixgbe: {}: Stop: released", at(b.location));
                Ok(())
            }
            None => {
                println!("stormnic-ixgbe: Stop for a controller this driver never started");
                Err(Status::DEVICE_ERROR.into())
            }
        }
    }
}
