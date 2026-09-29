//! `EFI_DRIVER_BINDING_PROTOCOL`: which controllers this driver takes.
//!
//! Supported looks at every PCI function the firmware offers. It reads the
//! vendor/device ID with a non-exclusive (GET_PROTOCOL) open, and only for one
//! of ours tries the BY_DRIVER open that Start will hold. A platform driver
//! that already owns the NIC holds that open, so ours fails and the platform's
//! driver wins: a medium carrying this driver is safe on a machine that has
//! its own.
//!
//! Start holds `EFI_PCI_IO_PROTOCOL` BY_DRIVER for the controller, enables
//! memory decode, and brings the NIC up (`hardware`: reset, NVM MAC, link
//! setup, a bounded wait for link). Stop restores the PCI attributes it
//! found and releases PciIo. The SNP child is later work (#3, #4).
//!
//! Every Intel network function Supported sees is logged, matched or not, so a
//! boot names the device ID the machine really has. Nothing else is: the
//! firmware calls Supported for every handle on every ConnectController.

use alloc::vec::Vec;
use core::time::Duration;
use uefi::boot::{self, OpenProtocolAttributes, OpenProtocolParams, ScopedProtocol};
use uefi::driver::Driver;
use uefi::proto::device_path::DevicePath;
use uefi::{println, Handle, Result, Status};

use crate::hardware::x552::{self, Copper, Module};
use crate::hardware::{self, Link, Registers, Setup};
use crate::ids::{self, Nic};
use crate::pci_io::{AttributeOp, Location, PciIo, ATTRIBUTE_MEMORY};

/// How long Start waits for an X557's copper link (10GBASE-T training takes
/// seconds) before the MAC link wait below.
const COPPER_WAIT_MS: usize = 5000;

/// How long Start waits for link before reporting it down. Link down is a
/// result: no cable, or a partner still negotiating (10GBASE-T takes seconds).
const LINK_WAIT_MS: usize = 3000;

/// The NIC's registers: memory BAR 0 through PciIo.
struct Bar0<'a>(&'a PciIo);

impl Registers for Bar0<'_> {
    type Error = Status;
    fn read(&mut self, offset: u32) -> core::result::Result<u32, Status> {
        self.0.mem_read_u32(0, offset).map_err(|e| e.status())
    }
    fn write(&mut self, offset: u32, value: u32) -> core::result::Result<(), Status> {
        self.0.mem_write_u32(0, offset, value).map_err(|e| e.status())
    }
    fn delay_us(&mut self, micros: usize) {
        boot::stall(Duration::from_micros(micros as u64));
    }
}

/// A controller this driver has started.
struct Bound {
    controller: Handle,
    location: Option<Location>,
    /// PCI attributes before Start enabled memory decode; Stop puts them back.
    attributes: u64,
    /// The BY_DRIVER open; dropping it closes the protocol.
    pci: ScopedProtocol<PciIo>,
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
        let attributes = match enable_memory(&pci) {
            Ok(a) => a,
            Err(e) => {
                println!(
                    "stormnic-ixgbe: {} 8086:{:04x}: Start could not enable memory decode: {:?}",
                    at(location),
                    nic.device,
                    e.status()
                );
                return Err(e);
            }
        };
        if let Err(e) = bring_up(&pci, nic, location) {
            println!(
                "stormnic-ixgbe: {} 8086:{:04x} {}: bring-up failed: {:x?}; releasing",
                at(location),
                nic.device,
                nic.name,
                e
            );
            restore(&pci, attributes);
            return Err(Status::DEVICE_ERROR.into());
        }
        self.bound.push(Bound { controller, location, attributes, pci });
        println!(
            "stormnic-ixgbe: {} 8086:{:04x} {}: Start: bound (no SNP yet)",
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
                restore(&b.pci, b.attributes);
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

/// Enable memory decode, returning the attributes to restore on release.
fn enable_memory(pci: &PciIo) -> Result<u64> {
    let original = pci.attributes(AttributeOp::Get, 0)?;
    let supported = pci.attributes(AttributeOp::Supported, 0)?;
    if supported & ATTRIBUTE_MEMORY == 0 {
        return Err(Status::UNSUPPORTED.into());
    }
    pci.attributes(AttributeOp::Enable, ATTRIBUTE_MEMORY)?;
    Ok(original)
}

fn restore(pci: &PciIo, attributes: u64) {
    if let Err(e) = pci.attributes(AttributeOp::Set, attributes) {
        println!("stormnic-ixgbe: could not restore PCI attributes {attributes:#x}: {:?}", e.status());
    }
}

/// Reset, NVM MAC, link setup and link wait, each step logged.
fn bring_up(pci: &PciIo, nic: &Nic, location: Option<Location>) -> core::result::Result<(), hardware::Error<Status>> {
    let mut io = Bar0(pci);
    let (at, dev) = (at(location), nic.device);
    let id = hardware::reset(&mut io)?;
    let m = id.mac;
    println!(
        "stormnic-ixgbe: {at} 8086:{dev:04x}: reset, LAN {}, MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        id.lan, m[0], m[1], m[2], m[3], m[4], m[5]
    );
    let setup = hardware::setup_link(&mut io, nic.family, dev, id.lan)?;
    match setup {
        Setup::Restarted { autoc, autoc2, esdp } => println!(
            "stormnic-ixgbe: {at} 8086:{dev:04x}: link setup: NVM mode {} (AUTOC {autoc:08x} AUTOC2 {autoc2:08x} ESDP {esdp:08x}), restarted",
            hardware::link_mode(autoc, autoc2)
        ),
        Setup::PhyAutonomous => println!(
            "stormnic-ixgbe: {at} 8086:{dev:04x}: link setup: integrated PHY auto-negotiates from its NVM image"
        ),
        Setup::X552(x) => log_x552(&at, dev, x),
    }
    if let Setup::X552(x552::Setup::Copper { phy, internal, .. }) = setup {
        match x552::follow_copper(&mut io, id.lan, phy, internal, COPPER_WAIT_MS)? {
            Copper::Down => println!("stormnic-ixgbe: {at} 8086:{dev:04x}: copper link down after {COPPER_WAIT_MS} ms"),
            Copper::Up { megabits, retrained } => println!(
                "stormnic-ixgbe: {at} 8086:{dev:04x}: copper link up {megabits} Mb/s{}",
                if retrained { ", internal iXFI re-forced to 1G" } else { "" }
            ),
            Copper::Unsupported { status } => println!(
                "stormnic-ixgbe: {at} 8086:{dev:04x}: copper link up at a speed the internal link cannot carry (AN vendor status {status:04x})"
            ),
        }
    }
    match hardware::wait_link(&mut io, LINK_WAIT_MS)? {
        Link::Up { megabits: Some(mb) } => println!("stormnic-ixgbe: {at} 8086:{dev:04x}: link up {mb} Mb/s"),
        Link::Up { megabits: None } => println!("stormnic-ixgbe: {at} 8086:{dev:04x}: link up, speed encoding reserved"),
        Link::Down => println!("stormnic-ixgbe: {at} 8086:{dev:04x}: link down after {LINK_WAIT_MS} ms"),
    }
    Ok(())
}

fn log_x552(at: &impl core::fmt::Display, dev: u16, setup: x552::Setup) {
    match setup {
        x552::Setup::Kx4 => println!(
            "stormnic-ixgbe: {at} 8086:{dev:04x}: link setup: KX4, run by the hardware; nothing written"
        ),
        x552::Setup::FirmwarePhy => println!(
            "stormnic-ixgbe: {at} 8086:{dev:04x}: link setup: external 1G PHY run by firmware; nothing written"
        ),
        x552::Setup::Kr { link_ctrl } => println!(
            "stormnic-ixgbe: {at} 8086:{dev:04x}: link setup: KR PHY auto-negotiating KR+KX (LINK_CTRL_1 {link_ctrl:08x}), restarted"
        ),
        x552::Setup::ManageabilityVeto => println!(
            "stormnic-ixgbe: {at} 8086:{dev:04x}: link setup: manageability veto (MMNGC.MNG_VETO); link left to firmware"
        ),
        x552::Setup::Sfp { module, cs4227_reset, link_ctrl, edc } => {
            let cs = if cs4227_reset { "CS4227 reset" } else { "CS4227 already reset" };
            match (module, link_ctrl, edc) {
                (Module::Absent, ..) => println!(
                    "stormnic-ixgbe: {at} 8086:{dev:04x}: link setup: {cs}; no SFP+ module"
                ),
                (Module::Unsupported { identifier, comp_10g, comp_1g, cable }, ..) => println!(
                    "stormnic-ixgbe: {at} 8086:{dev:04x}: link setup: {cs}; unsupported SFP module (id {identifier:02x}, 10G {comp_10g:02x}, 1G {comp_1g:02x}, cable {cable:02x}); link not set up"
                ),
                (m, Some(lc1), Some(edc)) => println!(
                    "stormnic-ixgbe: {at} 8086:{dev:04x}: link setup: {cs}; SFP {m:?}, KR PHY {} (LINK_CTRL_1 {lc1:08x}), CS4227 EDC {}",
                    if lc1 & (1 << 18) != 0 { "10G" } else { "1G" },
                    if edc == 2 { "CX1" } else { "SR" }
                ),
                (m, ..) => println!("stormnic-ixgbe: {at} 8086:{dev:04x}: link setup: {cs}; SFP {m:?}"),
            }
        }
        x552::Setup::Copper { phy, id, unstalled, internal } => println!(
            "stormnic-ixgbe: {at} 8086:{dev:04x}: link setup: X557 PHY {id:08x} at MDIO {phy}{}, internal link {internal:?}",
            if unstalled { ", power-up stall released" } else { "" }
        ),
    }
}
