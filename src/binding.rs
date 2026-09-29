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
//! memory decode, and brings the NIC up (`hardware`, in the order of
//! docs/spec/phy.md section 9: quiesce, the PHY/module steps before the MAC
//! reset, MAC reset and NVM MAC, link setup, a bounded wait for link). Stop
//! restores the PCI attributes it found and releases PciIo. The SNP child is
//! later work (#3, #4).
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

use crate::hardware::f82599::{self, Laser, PhyReset};
use crate::hardware::mdio::{self, Speeds};
use crate::hardware::sfp::{Kind, Module};
use crate::hardware::x552::{self, Copper};
use crate::hardware::{self, Error, Link, Port, Prepared, Registers, Setup};
use crate::ids::{self, Nic};
use crate::pci_io::{AttributeOp, Location, PciIo, ATTRIBUTE_MEMORY};

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

/// A PCI read or write failed, or the function is gone: stop bring-up.
/// Anything else in the PHY or link steps leaves the link to hardware and
/// firmware, and Start still reports what LINKS says (spec 1.4.3, 9).
fn fatal(e: &Error<Status>) -> bool { matches!(e, Error::Io(_) | Error::Removed) }

/// Quiesce, PHY/module steps, MAC reset, NVM MAC, link setup and link wait,
/// each step logged.
fn bring_up(pci: &PciIo, nic: &Nic, location: Option<Location>) -> core::result::Result<(), Error<Status>> {
    let mut io = Bar0(pci);
    let (at, dev) = (at(location), nic.device);
    let lan = hardware::begin(&mut io)?;
    let port = Port { family: nic.family, device: dev, lan };
    let veto = hardware::veto(&mut io)?;
    if veto {
        println!("stormnic-ixgbe: {at} 8086:{dev:04x}: manageability veto (MMNGC.MNG_VETO): no PHY reset, AN restart or link-mode write");
    }
    let prepared = match hardware::prepare(&mut io, port, veto) {
        Ok(p) => { log_prepared(&at, dev, &p); Some(p) }
        Err(e) if fatal(&e) => return Err(e),
        Err(e) => {
            println!("stormnic-ixgbe: {at} 8086:{dev:04x}: PHY/module check failed: {e:x?}; link left to hardware, reporting LINKS only");
            None
        }
    };
    let id = hardware::reset(&mut io, port)?;
    let m = id.mac;
    println!(
        "stormnic-ixgbe: {at} 8086:{dev:04x}: reset ({}), LAN {}, MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        if id.link_reset { "LNK_RST, link was down" } else { "RST" },
        id.lan, m[0], m[1], m[2], m[3], m[4], m[5]
    );
    let setup = match prepared.map(|p| hardware::setup_link(&mut io, port, veto, p)) {
        Some(Ok(s)) => { log_setup(&at, dev, &s); Some(s) }
        Some(Err(e)) if fatal(&e) => return Err(e),
        Some(Err(e)) => {
            println!("stormnic-ixgbe: {at} 8086:{dev:04x}: link setup failed: {e:x?}; reporting LINKS only");
            None
        }
        None => None,
    };
    let budget = hardware::link_budget_ms(port);
    let w = hardware::wait_link(&mut io, port, setup.as_ref(), budget)?;
    match w.copper {
        Some(Copper::Up { megabits }) => println!(
            "stormnic-ixgbe: {at} 8086:{dev:04x}: copper link up {megabits} Mb/s, internal link re-forced {} time(s)", w.reforced
        ),
        Some(Copper::Invalid { status }) => println!(
            "stormnic-ixgbe: {at} 8086:{dev:04x}: copper link up at a speed the internal link cannot carry (AN vendor status {status:04x})"
        ),
        Some(Copper::Down) => println!("stormnic-ixgbe: {at} 8086:{dev:04x}: copper link down"),
        None => {}
    }
    match w.link {
        Link::Up { megabits: Some(mb) } => println!("stormnic-ixgbe: {at} 8086:{dev:04x}: link up {mb} Mb/s"),
        Link::Up { megabits: None } => println!("stormnic-ixgbe: {at} 8086:{dev:04x}: link up, speed encoding reserved"),
        Link::Down => println!("stormnic-ixgbe: {at} 8086:{dev:04x}: link down after {budget} ms"),
    }
    Ok(())
}

fn speeds(s: Speeds) -> &'static str {
    match (s.g10, s.g1, s.m100) {
        (true, true, true) => "10G+1G+100M",
        (true, true, false) => "10G+1G",
        (true, false, true) => "10G+100M",
        (true, false, false) => "10G",
        (false, true, true) => "1G+100M",
        (false, true, false) => "1G",
        (false, false, true) => "100M",
        (false, false, false) => "nothing",
    }
}

fn module(m: &Module) -> impl core::fmt::Display + '_ {
    struct M<'a>(&'a Module);
    impl core::fmt::Display for M<'_> {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            let m = self.0;
            f.write_str(match m.kind {
                Kind::DaCu => "passive DA",
                Kind::DaActiveLimiting => "active limiting DA",
                Kind::SrLr => "10G SR/LR",
                Kind::Cu1g => "1000BASE-T",
                Kind::Sx1g => "1000BASE-SX",
                Kind::Lx1g => "1000BASE-LX",
                Kind::Bx10g => "10G BX",
                Kind::Bx1g => "1000BASE-BX",
                Kind::Unknown => "unknown",
                Kind::NotPresent => "none",
            })?;
            if m.multispeed { f.write_str(", multispeed")?; }
            write!(f, " (id {:02x}, 10G {:02x}, 1G {:02x}, cable {:02x})", m.identifier, m.comp_10g, m.comp_1g, m.cable)
        }
    }
    M(m)
}

fn log_prepared(at: &impl core::fmt::Display, dev: u16, p: &Prepared) {
    match p {
        Prepared::F82599(f82599::Prepared::Copper { phy, id, reset }) => println!(
            "stormnic-ixgbe: {at} 8086:{dev:04x}: {} PHY {id:08x} at MDIO {phy}, {}",
            mdio::name(*id),
            match reset {
                PhyReset::Done => "reset",
                PhyReset::Vetoed => "not reset (veto)",
                PhyReset::OverTemperature => "not reset (over-temperature alarm)",
            }
        ),
        Prepared::X552(x552::Prepared::Sfp { cs4227_reset, .. }) => println!(
            "stormnic-ixgbe: {at} 8086:{dev:04x}: {}",
            if *cs4227_reset { "CS4227 reset" } else { "CS4227 already reset" }
        ),
        Prepared::X552(x552::Prepared::Copper { phy, id, sel, unstalled, reset, .. }) => println!(
            "stormnic-ixgbe: {at} 8086:{dev:04x}: {} PHY {id:08x} at MDIO {phy} (NW_MNG_IF_SEL {sel:08x}){}, {}",
            mdio::name(*id),
            if *unstalled { ", power-up stall released" } else { "" },
            if *reset { "reset" } else { "not reset (veto)" }
        ),
        _ => {}
    }
}

fn log_setup(at: &impl core::fmt::Display, dev: u16, setup: &Setup) {
    match setup {
        Setup::F82599(s) => log_82599(at, dev, s),
        Setup::X540(s) => println!(
            "stormnic-ixgbe: {at} 8086:{dev:04x}: link setup: {} PHY {:08x} at MDIO {}, powered on, advertising {}, {}",
            mdio::name(s.id), s.id, s.phy, speeds(s.advertised),
            if s.restarted { "AN restarted" } else { "AN not restarted (veto)" }
        ),
        Setup::X552(x) => log_x552(at, dev, x),
    }
}

fn log_82599(at: &impl core::fmt::Display, dev: u16, setup: &f82599::Setup) {
    match *setup {
        f82599::Setup::Backplane { autoc, autoc2, written, an_complete } => println!(
            "stormnic-ixgbe: {at} 8086:{dev:04x}: link setup: NVM mode {} (AUTOC {autoc:08x} AUTOC2 {autoc2:08x}), {}{}",
            hardware::link_mode(autoc, autoc2),
            if written { "advertisement rewritten" } else { "already as the NVM set it" },
            match an_complete { Some(true) => ", AN complete", Some(false) => ", AN not complete after 4.5 s", None => "" }
        ),
        f82599::Setup::Module { module: ref m, autoc, autoc2, sequence: None, .. } => println!(
            "stormnic-ixgbe: {at} 8086:{dev:04x}: link setup: module {}{} (AUTOC {autoc:08x} AUTOC2 {autoc2:08x})",
            module(m),
            if m.present() { ", not supported; link not set up" } else { "" }
        ),
        f82599::Setup::Module { module: ref m, nvm_autoc, autoc, autoc2, sequence: Some(words), laser, speed, fw, crosstalk, rate_select } => {
            println!(
                "stormnic-ixgbe: {at} 8086:{dev:04x}: link setup: module {}, NVM init sequence {words} words, {} (NVM AUTOC {nvm_autoc:08x}, now AUTOC {autoc:08x} AUTOC2 {autoc2:08x}), laser {}{}{}",
                module(m),
                hardware::link_mode(autoc, autoc2),
                match laser {
                    Laser::On => "on",
                    Laser::NoDirection => "not driven (SDP3 is not an output)",
                    Laser::Manageability => "left to manageability",
                    Laser::None => "not controlled",
                },
                if crosstalk { ", cage-presence check on" } else { "" },
                if rate_select { "" } else { ", soft rate select failed" }
            );
            if let Some(v) = fw { println!("stormnic-ixgbe: {at} 8086:{dev:04x}: SFI firmware patch version {v:#x}{}", if v > 5 { "" } else { " (expected > 5)" }); }
            if let Some(mb) = speed { println!("stormnic-ixgbe: {at} 8086:{dev:04x}: multispeed: link at {mb} Mb/s"); }
        }
        f82599::Setup::Copper { phy, id, advertised, restarted, .. } => println!(
            "stormnic-ixgbe: {at} 8086:{dev:04x}: link setup: {} PHY {id:08x} at MDIO {phy}, advertising {}, {}",
            mdio::name(id), speeds(advertised),
            if restarted { "AN and MAC pipeline restarted" } else { "AN not restarted (veto)" }
        ),
    }
}

fn log_x552(at: &impl core::fmt::Display, dev: u16, setup: &x552::Setup) {
    match *setup {
        x552::Setup::Kx4 => println!(
            "stormnic-ixgbe: {at} 8086:{dev:04x}: link setup: KX4, run by the hardware; nothing written"
        ),
        x552::Setup::Xfi => println!(
            "stormnic-ixgbe: {at} 8086:{dev:04x}: link setup: XFI, run by the hardware; nothing written"
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
        x552::Setup::Sfp { module: ref m, link_ctrl: Some(lc1), edc: Some(edc), speed, rate_select, .. } => println!(
            "stormnic-ixgbe: {at} 8086:{dev:04x}: link setup: module {}, KR PHY {} (LINK_CTRL_1 {lc1:08x}), CS4227 EDC {}{}{}",
            module(m),
            if lc1 & (1 << 18) != 0 { "10G" } else { "1G" },
            if edc == 2 { "CX1" } else { "SR" },
            match speed { Some(10_000) => ", multispeed: link at 10G", Some(_) => ", multispeed: link at 1G", None => "" },
            if rate_select { "" } else { ", soft rate select failed" }
        ),
        x552::Setup::Sfp { module: ref m, .. } => println!(
            "stormnic-ixgbe: {at} 8086:{dev:04x}: link setup: module {}{}",
            module(m),
            if m.present() { ", not supported; link not set up" } else { "" }
        ),
        x552::Setup::Copper { internal, advertised, restarted, .. } => println!(
            "stormnic-ixgbe: {at} 8086:{dev:04x}: link setup: internal link {}, X557 advertising {}, {}",
            match internal { x552::Internal::Ixfi => "iXFI forced", x552::Internal::Kr => "KR (set at copper link-up)" },
            speeds(advertised),
            if restarted { "AN restarted" } else { "AN not restarted (veto)" }
        ),
    }
}
