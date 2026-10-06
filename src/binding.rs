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
//! memory decode and bus mastering (`decode`: PciIo attributes, falling back
//! to the command register on firmware that refuses them, #19), and brings the NIC up (`hardware`, in the
//! order of docs/spec/phy.md section 9: quiesce, the PHY/module steps before
//! the MAC reset, MAC reset and NVM MAC, link setup, a bounded wait for
//! link). It then maps the descriptor rings and buffers (`hardware::rings`,
//! #3), runs the DMA check on a link that is up, and stops the queues again.
//! Last it makes the SNP child handle (`snp`, #4): nothing DMAs until the
//! SNP's user (stormbootx's smoltcp) initializes it. Stop with children removes the child;
//! Stop without unmaps and frees the DMA region, undoes what Start did to
//! the PCI attributes and command register, and releases PciIo.
//!
//! The binding glue is here, not the `uefi` crate's `driver::install`: that
//! refuses Stop with children, which DisconnectController needs for the SNP
//! child.
//!
//! Every Intel network function Supported sees is logged, matched or not, so a
//! boot names the device ID the machine really has. Nothing else is: the
//! firmware calls Supported for every handle on every ConnectController.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::ffi::c_void;
use core::time::Duration;
use alloc::boxed::Box;
use uefi::boot::{self, OpenProtocolAttributes, OpenProtocolParams, ScopedProtocol};
use uefi::mem::memory_map::MemoryType;
use uefi::proto::loaded_image::LoadedImage;
use uefi::{Handle, Result, Status};
use uefi_raw::protocol::device_path::DevicePathProtocol;
use uefi_raw::protocol::driver::DriverBindingProtocol;

use crate::hardware::f82599::{self, Laser, PhyReset};
use crate::hardware::mdio::{self, Speeds};
use crate::hardware::rings::{self, Dma, Filter, Rings};
use crate::snp;
use crate::hardware::sfp::{Kind, Module};
use crate::hardware::x552::{self, Copper};
use crate::hardware::{self, Error, Link, Port, Prepared, Registers, Setup};
use crate::ids::{self, Nic};
use crate::console;
use crate::decode::{self, Enabled};
use crate::pci_io::{Location, PciIo};

/// The NIC's registers: memory BAR 0 through PciIo.
pub struct Bar0<'a>(pub &'a PciIo);

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
    /// How Start enabled memory decode and bus mastering; Stop undoes it.
    decode: Enabled<Status>,
    /// The descriptor rings and buffers, mapped for DMA.
    dma: DmaRegion,
    /// The SNP, its child handle and the rings (`snp::create`).
    port: *mut snp::Port,
    /// The BY_DRIVER open; dropping it closes the protocol.
    pci: ScopedProtocol<PciIo>,
}

/// `rings::DMA_PAGES` from AllocateBuffer, mapped as one common buffer.
struct DmaRegion {
    host: *mut u8,
    device: u64,
    mapping: *mut c_void,
}

impl DmaRegion {
    fn new(pci: &PciIo) -> Result<Self> {
        let host = pci.allocate_buffer(rings::DMA_PAGES)?;
        // SAFETY: `host` is DMA_PAGES pages from AllocateBuffer.
        match unsafe { pci.map_common(host, rings::DMA_PAGES * 4096) } {
            Ok((device, mapping)) => Ok(DmaRegion { host, device, mapping }),
            Err(e) => {
                // SAFETY: allocated above, never mapped or used.
                let _ = unsafe { pci.free_buffer(rings::DMA_PAGES, host) };
                Err(e)
            }
        }
    }

    /// Unmap and free. Only once the NIC's DMA is stopped.
    fn release(self, pci: &PciIo) {
        // SAFETY: the caller stopped the queues and disabled mastering, so
        // the device no longer reads or writes the region; nothing else
        // holds the pointer after this.
        unsafe {
            if let Err(e) = pci.unmap(self.mapping) {
                say!("stormnic-ixgbe: could not unmap the DMA region: {:?}", e.status());
            }
            if let Err(e) = pci.free_buffer(rings::DMA_PAGES, self.host) {
                say!("stormnic-ixgbe: could not free the DMA region: {:?}", e.status());
            }
        }
    }
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

pub fn at(location: Option<Location>) -> impl core::fmt::Display {
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
                trace!(
                    "stormnic-ixgbe: {} 8086:{:04x}: Intel network function, not in the 82599/X540/X552 list; not binding",
                    at(location),
                    id.device
                );
                Err(Status::UNSUPPORTED.into())
            }
        }
    }
}

impl IxgbeDriver {
    fn supported(&mut self, agent: Handle, controller: Handle) -> Result {
        if self.is_bound(controller) {
            return Err(Status::ALREADY_STARTED.into());
        }
        let (nic, location) = self.ours(agent, controller)?;
        match open(agent, controller, OpenProtocolAttributes::ByDriver) {
            Ok(_pci) => {
                trace!(
                    "stormnic-ixgbe: {} 8086:{:04x} {}: Supported",
                    at(location),
                    nic.device,
                    nic.name
                );
                Ok(())
            }
            Err(e) => {
                say!(
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

    fn start(&mut self, agent: Handle, controller: Handle) -> Result {
        console::begin();
        let (nic, location) = self.ours(agent, controller)?;
        let pci = match open(agent, controller, OpenProtocolAttributes::ByDriver) {
            Ok(pci) => pci,
            Err(e) => {
                fail!(
                    "stormnic-ixgbe: {} 8086:{:04x}: Start could not open PciIo BY_DRIVER: {:?}",
                    at(location),
                    nic.device,
                    e.status()
                );
                return Err(e);
            }
        };
        let attributes = match decode::enable(&*pci) {
            Ok(e) => {
                trace!("stormnic-ixgbe: {} 8086:{:04x}: {}", at(location), nic.device, Decode(&e));
                e
            }
            Err(decode::Error::Config(status)) => {
                fail!(
                    "stormnic-ixgbe: {} 8086:{:04x}: Start could not enable memory decode and bus mastering: \
                     command register access failed: {:?}",
                    at(location),
                    nic.device,
                    status
                );
                return Err(Status::UNSUPPORTED.into());
            }
            Err(decode::Error::NotSet(command, e)) => {
                fail!(
                    "stormnic-ixgbe: {} 8086:{:04x}: Start could not enable memory decode and bus mastering: \
                     command {command:#06x} after {}",
                    at(location),
                    nic.device,
                    Decode(&e)
                );
                return Err(Status::UNSUPPORTED.into());
            }
        };
        let (mac, link, state) = match bring_up(&pci, nic, location) {
            Ok(r) => r,
            Err(e) => {
                fail!(
                    "stormnic-ixgbe: {} 8086:{:04x} {}: bring-up failed: {:x?}; releasing",
                    at(location),
                    nic.device,
                    nic.name,
                    e
                );
                restore(&pci, &attributes);
                return Err(Status::DEVICE_ERROR.into());
            }
        };
        let (dma, rings) = match dma_up(&pci, nic, location, mac, link) {
            Ok(r) => r,
            Err(()) => {
                restore(&pci, &attributes);
                return Err(Status::DEVICE_ERROR.into());
            }
        };
        let media = link != Link::Down;
        let port = match snp::create(agent.as_ptr(), controller.as_ptr(), &pci, rings, nic.family, mac, media, location) {
            Ok(p) => p,
            Err((status, in_use)) => {
                fail!(
                    "stormnic-ixgbe: {} 8086:{:04x}: SNP not installed: {:?}; releasing",
                    at(location),
                    nic.device,
                    status
                );
                if in_use {
                    keep_dma(&pci, location, &Error::Io(status));
                } else {
                    dma.release(&pci);
                }
                restore(&pci, &attributes);
                return Err(Status::DEVICE_ERROR.into());
            }
        };
        self.bound.push(Bound { controller, location, decode: attributes, dma, port, pci });
        // The one default line per NIC (#22).
        say!(
            "stormnic-ixgbe {}: {} 8086:{:04x} {}: MAC {}, {}, SNP installed",
            env!("CARGO_PKG_VERSION"),
            at(location),
            nic.device,
            nic.name,
            mac_str(mac),
            state
        );
        console::begin();
        Ok(())
    }

    /// With `children`, remove the SNP child; without, release the NIC.
    fn stop(&mut self, agent: Handle, controller: Handle, children: &[uefi_raw::Handle]) -> Result {
        let Some(i) = self.bound.iter().position(|b| b.controller == controller) else {
            say!("stormnic-ixgbe: Stop for a controller this driver never started");
            return Err(Status::DEVICE_ERROR.into());
        };
        let (port, location) = (self.bound[i].port, self.bound[i].location);
        // SAFETY: `port` is live until `snp::destroy` below.
        let child = unsafe { (*port).child };
        if !children.is_empty() || child.is_some() {
            if child.is_some_and(|c| children.is_empty() || children.contains(&c)) {
                // SAFETY: as above.
                if let Err(s) = unsafe { snp::remove_child(agent.as_ptr(), controller.as_ptr(), port) } {
                    say!("stormnic-ixgbe: {}: Stop: SNP child still in use ({s:?}); kept", at(location));
                    return Err(Status::DEVICE_ERROR.into());
                }
                trace!("stormnic-ixgbe: {}: Stop: SNP child removed", at(location));
            }
            if !children.is_empty() {
                return Ok(());
            }
        }
        let b = self.bound.swap_remove(i);
        // SAFETY: the child is gone, so nothing else reaches the Port.
        match unsafe { snp::destroy(b.port) } {
            Ok(()) => b.dma.release(&b.pci),
            Err(e) => keep_dma(&b.pci, b.location, &e),
        }
        restore(&b.pci, &b.decode);
        trace!("stormnic-ixgbe: {}: Stop: released", at(b.location));
        Ok(())
    }
}

/// The driver binding interface and the driver behind it. `protocol` first:
/// firmware's `This` is the Binding.
#[repr(C)]
struct Binding {
    protocol: DriverBindingProtocol,
    driver: IxgbeDriver,
}

/// # Safety
/// `this` is the `protocol` of the leaked Binding; the firmware serializes
/// binding calls.
unsafe fn binding<'a>(this: *const DriverBindingProtocol) -> &'a mut Binding {
    unsafe { &mut *(this as *mut Binding) }
}

fn handles(this: *const DriverBindingProtocol, controller: uefi_raw::Handle) -> Option<(Handle, Handle)> {
    if this.is_null() { return None; }
    // SAFETY: the firmware passes our interface and a controller handle.
    unsafe {
        let agent = Handle::from_ptr((*this).driver_binding_handle)?;
        Some((agent, Handle::from_ptr(controller)?))
    }
}

unsafe extern "efiapi" fn binding_supported(this: *const DriverBindingProtocol, controller: uefi_raw::Handle,
    _remaining: *const DevicePathProtocol) -> Status {
    let Some((agent, controller)) = handles(this, controller) else { return Status::INVALID_PARAMETER };
    match unsafe { binding(this) }.driver.supported(agent, controller) {
        Ok(()) => Status::SUCCESS,
        Err(e) => e.status(),
    }
}

unsafe extern "efiapi" fn binding_start(this: *const DriverBindingProtocol, controller: uefi_raw::Handle,
    _remaining: *const DevicePathProtocol) -> Status {
    let Some((agent, controller)) = handles(this, controller) else { return Status::INVALID_PARAMETER };
    match unsafe { binding(this) }.driver.start(agent, controller) {
        Ok(()) => Status::SUCCESS,
        Err(e) => e.status(),
    }
}

unsafe extern "efiapi" fn binding_stop(this: *const DriverBindingProtocol, controller: uefi_raw::Handle,
    count: usize, children: *const uefi_raw::Handle) -> Status {
    let Some((agent, controller)) = handles(this, controller) else { return Status::INVALID_PARAMETER };
    if count > 0 && children.is_null() { return Status::INVALID_PARAMETER; }
    let children = if count == 0 { &[][..] } else {
        // SAFETY: the firmware passes `count` child handles.
        unsafe { core::slice::from_raw_parts(children, count) }
    };
    match unsafe { binding(this) }.driver.stop(agent, controller, children) {
        Ok(()) => Status::SUCCESS,
        Err(e) => e.status(),
    }
}

/// Install `EFI_DRIVER_BINDING_PROTOCOL` on the image handle. The image
/// must have been loaded as a boot-service driver (its code and data stay
/// after the entry point returns).
pub fn install() -> Result {
    let image = boot::image_handle();
    {
        let loaded = boot::open_protocol_exclusive::<LoadedImage>(image)?;
        if loaded.code_type() != MemoryType::BOOT_SERVICES_CODE || loaded.data_type() != MemoryType::BOOT_SERVICES_DATA {
            return Err(Status::UNSUPPORTED.into());
        }
    }
    let b = Box::into_raw(Box::new(Binding {
        protocol: DriverBindingProtocol {
            supported: binding_supported,
            start: binding_start,
            stop: binding_stop,
            version: 1,
            image_handle: image.as_ptr(),
            driver_binding_handle: image.as_ptr(),
        },
        driver: IxgbeDriver::new(),
    }));
    // SAFETY: the Binding is leaked, so the interface lives as long as the
    // image; the GUID matches the interface.
    let r = unsafe {
        boot::install_protocol_interface(Some(image), &DriverBindingProtocol::GUID, (&raw const (*b).protocol).cast())
    };
    if r.is_err() {
        // SAFETY: not installed, so nothing else holds it.
        drop(unsafe { Box::from_raw(b) });
    }
    r.map(|_| ())
}

/// `decode::enable`'s steps for the console: `PCI attributes Get G,
/// Supported S, Enable E: STATUS; command C` and, when the config write was
/// needed, `C0 -> C (set directly)`. A failed Get or Supported shows its
/// status in place of the value.
struct Decode<'a>(&'a Enabled<Status>);

impl core::fmt::Display for Decode<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let e = self.0;
        f.write_str("PCI attributes Get ")?;
        match e.original {
            Ok(v) => write!(f, "{v:#x}")?,
            Err(s) => write!(f, "{s:?}")?,
        }
        f.write_str(", Supported ")?;
        match e.supported {
            Ok(v) => write!(f, "{v:#x}")?,
            Err(s) => write!(f, "{s:?}")?,
        }
        match e.enable {
            Ok(()) => write!(f, ", Enable {:#x}: SUCCESS", e.by_attributes)?,
            Err(s) => write!(f, ", Enable: {s:?} (accepted alone {:#x})", e.by_attributes)?,
        }
        match e.command_before {
            Some(before) => write!(f, "; command {before:#06x} -> {:#06x} (set directly)", e.command),
            None => write!(f, "; command {:#06x}", e.command),
        }
    }
}

/// The queues could not be stopped: the NIC might still DMA into the
/// region, so it is never freed. Bus mastering is turned off at the PCI
/// level instead; the pages stay allocated until reboot.
fn keep_dma(pci: &PciIo, location: Option<Location>, e: &Error<Status>) {
    let off = decode::stop_bus_master(pci);
    fail!(
        "stormnic-ixgbe: {}: could not stop DMA: {e:x?}; bus mastering {}, DMA region kept allocated",
        at(location),
        if off.is_ok() { "disabled" } else { "could not be disabled" }
    );
}

/// Map the rings and buffers, start the queues, run the DMA check if the
/// link is up, and stop the queues. Everything is logged; Err(()) means
/// Start fails (the region is released, or kept if DMA could not stop).
fn dma_up(pci: &PciIo, nic: &Nic, location: Option<Location>, mac: [u8; 6], link: Link)
    -> core::result::Result<(DmaRegion, Rings), ()> {
    let (at, dev) = (at(location), nic.device);
    let region = match DmaRegion::new(pci) {
        Ok(r) => r,
        Err(e) => {
            fail!("stormnic-ixgbe: {at} 8086:{dev:04x}: DMA region not mapped: {:?}; releasing", e.status());
            return Err(());
        }
    };
    trace!(
        "stormnic-ixgbe: {at} 8086:{dev:04x}: DMA: {} pages at device {:#x}, RX {} x {} B, TX {} x {} B, legacy descriptors",
        rings::DMA_PAGES, region.device, rings::RX_DESCS, rings::BUF_SIZE, rings::TX_DESCS, rings::BUF_SIZE
    );
    // SAFETY: the region is DMA_BYTES (rounded to pages) from AllocateBuffer,
    // mapped at `device`, and only these rings use it until it is released.
    let mut rings = unsafe { Rings::new(Dma { host: region.host, device: region.device }) };
    let mut io = Bar0(pci);
    let filter = Filter { broadcast: true, ..Filter::default() };
    let checked = rings.start(&mut io, filter).and_then(|()| match link {
        Link::Up { .. } => rings::check(&mut io, &mut rings, mac, CHECK_LISTEN_MS).map(Some),
        Link::Down => Ok(None),
    });
    match &checked {
        Ok(Some(c)) => log_check(&at, dev, c),
        Ok(None) => trace!("stormnic-ixgbe: {at} 8086:{dev:04x}: rings started; DMA check skipped: link down"),
        Err(e) => fail!("stormnic-ixgbe: {at} 8086:{dev:04x}: rings failed: {e:x?}"),
    }
    match rings.stop(&mut io) {
        Ok(left) if checked.is_ok() => {
            note!(left > 0, 
                "stormnic-ixgbe: {at} 8086:{dev:04x}: rings stopped{}",
                if left > 0 { " (a frame was never sent)" } else { "" }
            );
            Ok((region, rings))
        }
        Ok(_) => {
            region.release(pci);
            say!("stormnic-ixgbe: {at} 8086:{dev:04x}: DMA region released; releasing");
            Err(())
        }
        Err(e) => {
            keep_dma(pci, location, &e);
            Err(())
        }
    }
}

/// How long the DMA check listens for a frame from the network.
const CHECK_LISTEN_MS: usize = 3000;

fn mac_str(m: [u8; 6]) -> impl core::fmt::Display {
    struct M([u8; 6]);
    impl core::fmt::Display for M {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            let m = self.0;
            write!(f, "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5])
        }
    }
    M(m)
}

fn log_check(at: &impl core::fmt::Display, dev: u16, c: &rings::Checked) {
    note!(!c.sent, 
        "stormnic-ixgbe: {at} 8086:{dev:04x}: DMA check: broadcast frame {} (GPTC {})",
        if c.sent { "sent, 60 bytes" } else { "not sent within 100 ms" },
        c.gptc
    );
    match c.first {
        Some(f) => trace!(
            "stormnic-ixgbe: {at} 8086:{dev:04x}: DMA check: received {} frame(s) after {} ms (GPRC {}), first {} bytes from {} to {} type {:04x}",
            c.received, c.waited_ms, c.gprc, f.len, mac_str(f.source), mac_str(f.destination), f.ethertype
        ),
        None => trace!(
            "stormnic-ixgbe: {at} 8086:{dev:04x}: DMA check: received nothing in {} ms (GPRC {})",
            c.waited_ms, c.gprc
        ),
    }
}

fn restore(pci: &PciIo, e: &Enabled<Status>) {
    if let Err(s) = decode::release(pci, e) {
        say!("stormnic-ixgbe: could not undo the PCI decode and bus-master changes: {s:?}");
    }
}

/// A PCI read or write failed, or the function is gone: stop bring-up.
/// Anything else in the PHY or link steps leaves the link to hardware and
/// firmware, and Start still reports what LINKS says (spec 1.4.3, 9).
fn fatal(e: &Error<Status>) -> bool { matches!(e, Error::Io(_) | Error::Removed) }

/// Quiesce, PHY/module steps, MAC reset, NVM MAC, link setup and link wait,
/// each step logged.
fn bring_up(pci: &PciIo, nic: &Nic, location: Option<Location>) -> core::result::Result<([u8; 6], Link, String), Error<Status>> {
    let mut io = Bar0(pci);
    let (at, dev) = (at(location), nic.device);
    let lan = hardware::begin(&mut io)?;
    let port = Port { family: nic.family, device: dev, lan };
    let veto = hardware::veto(&mut io)?;
    if veto {
        say!("stormnic-ixgbe: {at} 8086:{dev:04x}: manageability veto (MMNGC.MNG_VETO): no PHY reset, AN restart or link-mode write");
    }
    let prepared = match hardware::prepare(&mut io, port, veto) {
        Ok(p) => { log_prepared(&at, dev, &p); Some(p) }
        Err(e) if fatal(&e) => return Err(e),
        Err(e) => {
            fail!("stormnic-ixgbe: {at} 8086:{dev:04x}: PHY/module check failed: {e:x?}; link left to hardware, reporting LINKS only");
            None
        }
    };
    let id = hardware::reset(&mut io, port)?;
    let m = id.mac;
    trace!(
        "stormnic-ixgbe: {at} 8086:{dev:04x}: reset ({}), LAN {}, MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        if id.link_reset { "LNK_RST, link was down" } else { "RST" },
        id.lan, m[0], m[1], m[2], m[3], m[4], m[5]
    );
    if let Some(last) = id.cfg_pending {
        trace!(
            "stormnic-ixgbe: {at} 8086:{dev:04x}: EEMNGCTL CFG_DONE{} not set after 1 s (EEMNGCTL {last:#010x}); NVM auto-read done, continuing",
            id.lan
        );
    }
    let setup = match prepared.map(|p| hardware::setup_link(&mut io, port, veto, p)) {
        Some(Ok(s)) => { log_setup(&at, dev, &s); Some(s) }
        Some(Err(e)) if fatal(&e) => return Err(e),
        Some(Err(e)) => {
            fail!("stormnic-ixgbe: {at} 8086:{dev:04x}: link setup failed: {e:x?}; reporting LINKS only");
            None
        }
        None => None,
    };
    let budget = hardware::link_budget_ms(port);
    let w = hardware::wait_link(&mut io, port, setup.as_ref(), budget)?;
    match w.copper {
        Some(Copper::Up { megabits }) => trace!(
            "stormnic-ixgbe: {at} 8086:{dev:04x}: copper link up {megabits} Mb/s, internal link re-forced {} time(s)", w.reforced
        ),
        Some(Copper::Invalid { status }) => say!(
            "stormnic-ixgbe: {at} 8086:{dev:04x}: copper link up at a speed the internal link cannot carry (AN vendor status {status:04x})"
        ),
        Some(Copper::Down) => trace!("stormnic-ixgbe: {at} 8086:{dev:04x}: copper link down"),
        None => {}
    }
    // The summary line carries this (#22); verbose prints it here as well.
    let state = match w.link {
        Link::Up { megabits: Some(mb) } => format!("link up {mb} Mb/s"),
        Link::Up { megabits: None } => String::from("link up, speed encoding reserved"),
        Link::Down => {
            let [links, autoc, autoc2, esdp] = hardware::link_registers(&mut io)?;
            format!("link down after {budget} ms (LINKS {links:08x}, AUTOC {autoc:08x}, AUTOC2 {autoc2:08x}, ESDP {esdp:08x})")
        }
    };
    trace!("stormnic-ixgbe: {at} 8086:{dev:04x}: {state}");
    Ok((id.mac, w.link, state))
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
        Prepared::F82599(f82599::Prepared::Copper { phy, id, reset }) => note!(*reset == PhyReset::OverTemperature, 
            "stormnic-ixgbe: {at} 8086:{dev:04x}: {} PHY {id:08x} at MDIO {phy}, {}",
            mdio::name(*id),
            match reset {
                PhyReset::Done => "reset",
                PhyReset::Vetoed => "not reset (veto)",
                PhyReset::OverTemperature => "not reset (over-temperature alarm)",
            }
        ),
        Prepared::X552(x552::Prepared::Sfp { cs4227_reset, .. }) => trace!(
            "stormnic-ixgbe: {at} 8086:{dev:04x}: {}",
            if *cs4227_reset { "CS4227 reset" } else { "CS4227 already reset" }
        ),
        Prepared::X552(x552::Prepared::Copper { phy, id, sel, unstalled, reset, .. }) => trace!(
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
        Setup::X540(s) => trace!(
            "stormnic-ixgbe: {at} 8086:{dev:04x}: link setup: {} PHY {:08x} at MDIO {}, powered on, advertising {}, {}",
            mdio::name(s.id), s.id, s.phy, speeds(s.advertised),
            if s.restarted { "AN restarted" } else { "AN not restarted (veto)" }
        ),
        Setup::X552(x) => log_x552(at, dev, x),
    }
}

fn log_82599(at: &impl core::fmt::Display, dev: u16, setup: &f82599::Setup) {
    match *setup {
        f82599::Setup::Backplane { autoc, autoc2, written, an_complete } => trace!(
            "stormnic-ixgbe: {at} 8086:{dev:04x}: link setup: NVM mode {} (AUTOC {autoc:08x} AUTOC2 {autoc2:08x}), {}{}",
            hardware::link_mode(autoc, autoc2),
            if written { "advertisement rewritten" } else { "already as the NVM set it" },
            match an_complete { Some(true) => ", AN complete", Some(false) => ", AN not complete after 4.5 s", None => "" }
        ),
        f82599::Setup::Module { module: ref m, autoc, autoc2, sequence: None, .. } => note!(m.present(), 
            "stormnic-ixgbe: {at} 8086:{dev:04x}: link setup: module {}{} (AUTOC {autoc:08x} AUTOC2 {autoc2:08x})",
            module(m),
            if m.present() { ", not supported; link not set up" } else { "" }
        ),
        f82599::Setup::Module { module: ref m, nvm_autoc, autoc, autoc2, sequence: Some(words), laser, speed, fw, crosstalk, rate_select } => {
            note!(!rate_select, 
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
            if let Some(v) = fw { note!(v <= 5, "stormnic-ixgbe: {at} 8086:{dev:04x}: SFI firmware patch version {v:#x}{}", if v > 5 { "" } else { " (expected > 5)" }); }
            match speed {
                Some(mb) => trace!("stormnic-ixgbe: {at} 8086:{dev:04x}: multispeed: link at {mb} Mb/s"),
                None if m.multispeed || f82599::media(dev) == f82599::Media::FiberFixed => trace!(
                    "stormnic-ixgbe: {at} 8086:{dev:04x}: multispeed: no link at 10G or 1G; left at 10G"
                ),
                None => {}
            }
        }
        f82599::Setup::Copper { phy, id, advertised, restarted, .. } => trace!(
            "stormnic-ixgbe: {at} 8086:{dev:04x}: link setup: {} PHY {id:08x} at MDIO {phy}, advertising {}, {}",
            mdio::name(id), speeds(advertised),
            if restarted { "AN and MAC pipeline restarted" } else { "AN not restarted (veto)" }
        ),
    }
}

fn log_x552(at: &impl core::fmt::Display, dev: u16, setup: &x552::Setup) {
    match *setup {
        x552::Setup::Kx4 => trace!(
            "stormnic-ixgbe: {at} 8086:{dev:04x}: link setup: KX4, run by the hardware; nothing written"
        ),
        x552::Setup::Xfi => trace!(
            "stormnic-ixgbe: {at} 8086:{dev:04x}: link setup: XFI, run by the hardware; nothing written"
        ),
        x552::Setup::FirmwarePhy => trace!(
            "stormnic-ixgbe: {at} 8086:{dev:04x}: link setup: external 1G PHY run by firmware; nothing written"
        ),
        x552::Setup::Kr { link_ctrl } => trace!(
            "stormnic-ixgbe: {at} 8086:{dev:04x}: link setup: KR PHY auto-negotiating KR+KX (LINK_CTRL_1 {link_ctrl:08x}), restarted"
        ),
        x552::Setup::ManageabilityVeto => trace!(
            "stormnic-ixgbe: {at} 8086:{dev:04x}: link setup: manageability veto (MMNGC.MNG_VETO); link left to firmware"
        ),
        x552::Setup::Sfp { module: ref m, link_ctrl: Some(lc1), edc: Some(edc), speed, rate_select, crosstalk, .. } => note!(!rate_select || crosstalk.is_none(), 
            "stormnic-ixgbe: {at} 8086:{dev:04x}: link setup: module {}, KR PHY {} (LINK_CTRL_1 {lc1:08x}), CS4227 EDC {}{}{}{}",
            module(m),
            if lc1 & (1 << 18) != 0 { "10G" } else { "1G" },
            if edc == 2 { "CX1" } else { "SR" },
            match speed { Some(10_000) => ", multispeed: link at 10G", Some(_) => ", multispeed: link at 1G", None => "" },
            if rate_select { "" } else { ", soft rate select failed" },
            match crosstalk {
                Some(true) => ", cage-presence check on",
                Some(false) => "",
                None => ", NVM word 0x2C unreadable (host interface), cage-presence check off",
            }
        ),
        x552::Setup::Sfp { module: ref m, .. } => note!(m.present(), 
            "stormnic-ixgbe: {at} 8086:{dev:04x}: link setup: module {}{}",
            module(m),
            if m.present() { ", not supported; link not set up" } else { "" }
        ),
        x552::Setup::Copper { internal, advertised, restarted, .. } => trace!(
            "stormnic-ixgbe: {at} 8086:{dev:04x}: link setup: internal link {}, X557 advertising {}, {}",
            match internal { x552::Internal::Ixfi => "iXFI forced", x552::Internal::Kr => "KR (set at copper link-up)" },
            speeds(advertised),
            if restarted { "AN restarted" } else { "AN not restarted (veto)" }
        ),
    }
}
