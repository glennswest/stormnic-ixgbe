//! Bring-up: reset and NVM MAC from the datasheets; PHY and link
//! programming per docs/spec/phy.md (the independent specification written
//! from Intel's BSD-licensed shared code, #13). Section numbers in comments
//! ("spec 5.4") point into that document.
//!
//! Kept independent of UEFI so failure sequences can be tested without a NIC.
//! The caller owns PciIo BY_DRIVER, enables BAR memory access, and supplies
//! firmware Stall for delays. Bring-up needs no DMA; `rings` (#3) takes a
//! DMA region the caller has mapped, with bus mastering enabled.
//!
//! Order, per spec 9: `begin` (port number, quiesce), `veto` (MMNGC once),
//! `prepare` (the PHY or module steps the spec puts before the MAC reset),
//! `reset` (MAC reset per family, NVM MAC), `setup_link`, then `wait_link`.

pub const CTRL: u32 = 0x00000;
pub const STATUS: u32 = 0x00008;
pub const ESDP: u32 = 0x00020;
pub const HLREG0: u32 = 0x04240;
pub const AUTOC: u32 = 0x042a0;
pub const LINKS: u32 = 0x042a4;
pub const AUTOC2: u32 = 0x042a8;
pub const MMNGC: u32 = 0x042d0;
pub const EERD: u32 = 0x10014;
const RXCTRL: u32 = 0x03000;
const EIMC: u32 = 0x00888;
const EEC: u32 = 0x10010;
const EEMNGCTL: u32 = 0x10110;
const RDRXCTL: u32 = 0x02f00;
const RAL0: u32 = 0x0a200;
const RAH0: u32 = 0x0a204;
const MASTER_DISABLE: u32 = 1 << 2;
const MASTER_ENABLED: u32 = 1 << 19;
const RST: u32 = 1 << 26;
const LNK_RST: u32 = 1 << 3;
const QUEUE_ENABLE: u32 = 1 << 25;
const LINK_UP: u32 = 1 << 30;
const MNG_VETO: u32 = 1 << 0;

pub trait Registers {
    type Error;
    fn read(&mut self, offset: u32) -> Result<u32, Self::Error>;
    fn write(&mut self, offset: u32, value: u32) -> Result<(), Self::Error>;
    fn delay_us(&mut self, micros: usize);
}

#[derive(Debug, PartialEq, Eq)]
pub enum Error<E> {
    Io(E),
    Timeout { register: u32, mask: u32, expected: u32, last: u32 },
    Removed,
    VirtualizationActive,
    InvalidPort,
    MissingNvm,
    InvalidMac,
    /// SW_FW_SYNC resource (or SWSM.SMBI / SWESMBI / REGSMP) still held by
    /// firmware or another driver after the bounded wait; never taken from
    /// its owner (spec 1.4.3 policy, section 10 item 15).
    Semaphore { held: u32 },
    /// IOSF sideband access to the X552 KR PHY returned an error response.
    Sideband { address: u32, ctrl: u32 },
    /// No ACK from I2C device `device` (8-bit address) after retries.
    I2c { device: u8 },
    /// CS4227 reset did not load its image (register and last value read).
    Cs4227 { register: u16, value: u16 },
    /// No external PHY answered on MDIO.
    NoPhy,
    /// X552 firmware host interface: disabled (HICR.EN clear), or the
    /// command did not complete with a valid status (spec 11.2).
    HostInterface { hicr: u32 },
    /// PHY soft reset (4.0x0000 bit 15) still set after 3 s (spec 2.6).
    PhyReset,
    /// 82599: the NVM has no init sequence for this module type (spec 5.5).
    NoInitSequence { key: u16 },
    /// 82599: the AN state (ANLP1 19:16) never left 0 in a pipeline reset (spec 5.4).
    PipelineReset,
    /// A frame outside 14..=1518 bytes to send, or a received frame longer
    /// than the caller's buffer (its length; it stays queued).
    FrameLength { len: usize },
}

#[path = "sync.rs"]
pub mod sync;
#[path = "mdio.rs"]
pub mod mdio;
#[path = "i2c.rs"]
pub mod i2c;
#[path = "sfp.rs"]
pub mod sfp;
#[path = "f82599.rs"]
pub mod f82599;
#[path = "x540.rs"]
pub mod x540;
#[path = "x552.rs"]
pub mod x552;
#[path = "rings.rs"]
pub mod rings;
#[path = "snp_core.rs"]
pub mod snp;

type R<T, E> = Result<T, Error<E>>;

fn read<Io: Registers>(io: &mut Io, reg: u32) -> R<u32, Io::Error> {
    let value = io.read(reg).map_err(Error::Io)?;
    // These status/control registers cannot legitimately have all bits set.
    // Do not interpret a disappeared PCI function as reset/NVM/link success.
    if value == u32::MAX { return Err(Error::Removed); }
    Ok(value)
}

fn write<Io: Registers>(io: &mut Io, reg: u32, value: u32) -> R<(), Io::Error> {
    io.write(reg, value).map_err(Error::Io)
}

/// A read of STATUS so a write reaches the device before a delay starts.
fn flush<Io: Registers>(io: &mut Io) -> R<(), Io::Error> {
    io.read(STATUS).map(|_| ()).map_err(Error::Io)
}

fn delay_ms<Io: Registers>(io: &mut Io, ms: usize) { io.delay_us(ms * 1000); }

/// Poll at 1 ms intervals, including a last read at the deadline.
fn wait<Io: Registers>(io: &mut Io, reg: u32, mask: u32, expected: u32,
    millis: usize) -> R<u32, Io::Error> {
    for elapsed in 0..=millis {
        let value = read(io, reg)?;
        if value & mask == expected { return Ok(value); }
        if elapsed == millis {
            return Err(Error::Timeout { register: reg, mask, expected, last: value });
        }
        io.delay_us(1000);
    }
    unreachable!()
}

fn mask_interrupts<Io: Registers>(io: &mut Io) -> R<(), Io::Error> {
    write(io, EIMC, 0x7fff_ffff)?;
    write(io, 0x00ab0, u32::MAX)?;
    write(io, 0x00ab4, u32::MAX)
}

fn rxdctl(queue: u32) -> u32 {
    if queue < 64 { 0x01028 + queue * 0x40 }
    else { 0x0d028 + (queue - 64) * 0x40 }
}

/// Controller family (spec 1.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family { F82599, X540, X552 }

/// The function being brought up: its family, device ID and port number
/// (STATUS.LAN_ID, spec 1.3: every per-port choice uses it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Port { pub family: Family, pub device: u16, pub lan: u8 }

/// Stop host reception and drain outstanding PCIe requests before reset.
/// A drain timeout fails closed; no forced reset or stealing another owner's
/// semaphore is attempted. Interrupts and reception remain disabled on error.
pub fn quiesce<Io: Registers>(io: &mut Io) -> R<(), Io::Error> {
    if read(io, STATUS)? & (1 << 18) != 0 {
        return Err(Error::VirtualizationActive);
    }
    mask_interrupts(io)?;
    let rx = read(io, RXCTRL)?;
    write(io, RXCTRL, rx & !1)?;
    // Issue all disables first so the timeout is shared across queues.
    for queue in 0..128 {
        let reg = rxdctl(queue);
        let value = read(io, reg)?;
        if value & QUEUE_ENABLE != 0 { write(io, reg, value & !QUEUE_ENABLE)?; }
    }
    for elapsed in 0..=100 {
        let mut pending = None;
        for queue in 0..128 {
            let reg = rxdctl(queue);
            let value = read(io, reg)?;
            if value & QUEUE_ENABLE != 0 { pending = Some((reg, value)); break; }
        }
        match pending {
            None => break,
            Some((register, last)) if elapsed == 100 => return Err(Error::Timeout {
                register, mask: QUEUE_ENABLE, expected: 0, last,
            }),
            Some(_) => io.delay_us(1000),
        }
    }
    let ctrl = read(io, CTRL)?;
    write(io, CTRL, ctrl | MASTER_DISABLE)?;
    // CTRL readback must precede STATUS polling (PCIe write completion).
    wait(io, CTRL, MASTER_DISABLE, MASTER_DISABLE, 100)?;
    wait(io, STATUS, MASTER_ENABLED, 0, 100)?;
    Ok(())
}

/// The port number (spec 1.3), then `quiesce`. Nothing is reset yet.
pub fn begin<Io: Registers>(io: &mut Io) -> R<u8, Io::Error> {
    let lan = ((read(io, STATUS)? >> 2) & 3) as u8;
    if lan > 1 { return Err(Error::InvalidPort); }
    quiesce(io)?;
    Ok(lan)
}

/// MMNGC.MNG_VETO (spec 1.5), read once before any disruptive step. While
/// set: no PHY reset, no PHY AN restart, no 82599 AUTOC write, no X552 KR
/// setup, and the SFP+ laser is not dropped.
pub fn veto<Io: Registers>(io: &mut Io) -> R<bool, Io::Error> {
    Ok(read(io, MMNGC)? & MNG_VETO != 0)
}

/// The PHY or module steps before the MAC reset (spec 9.1, 9.3, 9.4, 9.8, 9.9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prepared {
    F82599(f82599::Prepared),
    X540,
    X552(x552::Prepared),
}

pub fn prepare<Io: Registers>(io: &mut Io, port: Port, veto: bool) -> R<Prepared, Io::Error> {
    Ok(match port.family {
        Family::F82599 => Prepared::F82599(f82599::prepare(io, port, veto)?),
        Family::X540 => Prepared::X540,
        Family::X552 => Prepared::X552(x552::prepare(io, port, veto)?),
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Identity {
    pub lan: u8,
    /// NVM-provisioned address reloaded into RAR0 by this reset. Never taken
    /// from RAR0 before reset, where an earlier driver may have replaced it.
    pub mac: [u8; 6],
    /// The reset used: CTRL.RST (link was up, or X540) or CTRL.LNK_RST.
    pub link_reset: bool,
    /// EEMNGCTL as last read when this port's CFG_DONE bit stayed clear
    /// for the whole 1 s wait; None when it set. Not fatal (#21): on the
    /// X9 blades, whose 82599 is shared with the BMC, it never sets, while
    /// EEC.AUTO_RD and EE_PRES have already confirmed the NVM load.
    pub cfg_pending: Option<u32>,
}

/// MAC reset (spec 5.13, 6.7, 7.11), NVM completion, and the per-port MAC.
/// The EEMNGCTL.CFG_DONE wait is bounded and only reported (`cfg_pending`).
///
/// CTRL.LNK_RST when the link is down, CTRL.RST when it is up (a link reset
/// could reset a PHY manageability is using); the X540 always uses RST. The
/// X540 and the X552 10G_T and SFP devices reset holding their PHY semaphore
/// (spec 1.4.4). Leaves all host interrupts masked and RX/TX disabled.
pub fn reset<Io: Registers>(io: &mut Io, port: Port) -> R<Identity, Io::Error> {
    let up = read(io, LINKS)? & LINK_UP != 0;
    let bits = if port.family == Family::X540 || up { RST } else { LNK_RST };
    let mask = match port.family {
        Family::F82599 => 0,
        Family::X540 => sync::phy(port.lan),
        Family::X552 => x552::reset_mask(port),
    };
    let ctrl = read(io, CTRL)?;
    if mask != 0 { sync::acquire(io, port, mask)?; }
    let written = write(io, CTRL, ctrl | bits);
    // Datasheet forbids even a flush read in the first millisecond.
    io.delay_us(1000);
    if mask != 0 { sync::release(io, port, mask)?; }
    written?;
    wait(io, CTRL, RST | LNK_RST, 0, 100)?;
    delay_ms(io, if port.family == Family::X540 { 100 } else { 50 });
    mask_interrupts(io)?;
    let eec = wait(io, EEC, 1 << 9, 1 << 9, 1000)?;
    // AUTO_RD also sets for absent or invalid NVM; require EE_PRES as well.
    if eec & (1 << 8) == 0 { return Err(Error::MissingNvm); }
    // CFG_DONE0/1 (bit 18 + LAN_ID): this port's configuration load.
    let cfg = 1 << (18 + port.lan);
    let cfg_pending = match wait(io, EEMNGCTL, cfg, cfg, 1000) {
        Ok(_) => None,
        Err(Error::Timeout { last, .. }) => Some(last),
        Err(e) => return Err(e),
    };
    wait(io, RDRXCTL, 1 << 3, 1 << 3, 1000)?;
    match port.family {
        Family::F82599 => f82599::after_reset(io)?,
        Family::X540 => {}
        Family::X552 => x552::after_reset(io, port)?,
    }
    // Unlike control/status registers, RAL can legitimately be all ones.
    let low = io.read(RAL0).map_err(Error::Io)?.to_le_bytes();
    let high = read(io, RAH0)?;
    let mac = [low[0], low[1], low[2], low[3], high as u8, (high >> 8) as u8];
    if high & (1 << 31) == 0 || mac == [0; 6] || mac[0] & 1 != 0 {
        return Err(Error::InvalidMac);
    }
    Ok(Identity { lan: port.lan, mac, link_reset: bits == LNK_RST, cfg_pending })
}

/// One 16-bit NVM word through EERD (82599 datasheet 8.2.3.2.2: bit 0
/// START, bit 1 DONE, bits 15:2 word address, bits 31:16 data).
pub fn nvm_word<Io: Registers>(io: &mut Io, word: u32) -> R<u16, Io::Error> {
    write(io, EERD, (word & 0x3fff) << 2 | 1)?;
    let mut last = 0;
    for _ in 0..10_000 {
        last = read(io, EERD)?;
        if last & 2 != 0 { return Ok((last >> 16) as u16); }
        io.delay_us(10);
    }
    Err(Error::Timeout { register: EERD, mask: 2, expected: 2, last })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Setup {
    F82599(f82599::Setup),
    X540(x540::Setup),
    X552(x552::Setup),
}

/// Link setup after `reset`, per family (spec 9).
pub fn setup_link<Io: Registers>(io: &mut Io, port: Port, veto: bool, prepared: Prepared)
    -> R<Setup, Io::Error> {
    Ok(match prepared {
        Prepared::F82599(p) => Setup::F82599(f82599::setup(io, port, veto, p)?),
        Prepared::X540 => Setup::X540(x540::setup(io, port, veto)?),
        Prepared::X552(p) => Setup::X552(x552::setup(io, port, veto, p)?),
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Link {
    Down,
    /// Full duplex (spec 8.1). None: up, but the speed encoding is reserved.
    Up { megabits: Option<u32> },
}

/// LINKS (spec 8.1): the current LINK_UP bit, not the latched history. A
/// cable unplugged is a normal state, not a bring-up timeout. Bits 29:28
/// are the speed; on the X552, `11` with NON_STD (bit 27) is 2.5G.
pub fn link<Io: Registers>(io: &mut Io, family: Family) -> R<Link, Io::Error> {
    let value = read(io, LINKS)?;
    if value & LINK_UP == 0 { return Ok(Link::Down); }
    let megabits = match (value >> 28) & 3 {
        1 => Some(100),
        2 => Some(1000),
        3 if family == Family::X552 && value & (1 << 27) != 0 => Some(2500),
        3 => Some(10_000),
        _ => None,
    };
    Ok(Link::Up { megabits })
}

/// LINKS behind the crosstalk fix (spec 4.4, 8.1): when `crosstalk` is on,
/// an empty cage (82599 SDP2, X552 SDP0 clear) is link down, and "up" is
/// read again after 5 ms.
pub fn cage_link<Io: Registers>(io: &mut Io, family: Family, crosstalk: bool) -> R<Link, Io::Error> {
    let cage = if family == Family::X552 { 1 << 0 } else { 1 << 2 };
    if crosstalk && read(io, ESDP)? & cage == 0 { return Ok(Link::Down); }
    let state = link(io, family)?;
    if crosstalk && state != Link::Down {
        delay_ms(io, 5);
        return link(io, family);
    }
    Ok(state)
}

/// LINKS, AUTOC, AUTOC2 and ESDP as they are, for the console when the link
/// stays down (#26): the PMD signal-detect and PCS sync fields of LINKS say
/// whether anything is heard from the far end, AUTOC/AUTOC2 the mode left
/// programmed, ESDP the SDP pins (laser, cage presence, rate select).
pub fn link_registers<Io: Registers>(io: &mut Io) -> R<[u32; 4], Io::Error> {
    Ok([read(io, LINKS)?, read(io, AUTOC)?, read(io, AUTOC2)?, read(io, ESDP)?])
}

/// How long to wait for link: copper (10GBASE-T AN and training take
/// seconds) gets the shared code's 9 s; fiber and backplane 3 s (spec 8.1,
/// 9.1, 9.5).
pub fn link_budget_ms(port: Port) -> usize {
    let copper = match port.family {
        Family::F82599 => f82599::media(port.device) == f82599::Media::Copper,
        Family::X540 => true,
        Family::X552 => port.device == 0x15ad,
    };
    if copper { 9000 } else { 3000 }
}

/// What `wait_link` saw: the MAC link, and on the X552 10G_T the copper side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Waited {
    pub link: Link,
    pub copper: Option<x552::Copper>,
    /// X552 10G_T: times the internal link was re-forced to the copper speed.
    pub reforced: u32,
}

/// Poll every 100 ms for up to `millis` (spec 8.1, 9). Link down at the end
/// is a result, not an error. `setup` is None when link setup was skipped or
/// failed: then only LINKS is read ("hands off", spec 1.4.3).
pub fn wait_link<Io: Registers>(io: &mut Io, port: Port, setup: Option<&Setup>, millis: usize)
    -> R<Waited, Io::Error> {
    let mut watch = match setup {
        Some(Setup::X552(x552::Setup::Copper { phy, internal, .. })) => Some(x552::Watch::new(*phy, *internal)),
        _ => None,
    };
    let crosstalk = matches!(setup, Some(Setup::F82599(f82599::Setup::Module { crosstalk: true, .. }))
        | Some(Setup::X552(x552::Setup::Sfp { crosstalk: Some(true), .. })));
    let mut elapsed = 0;
    loop {
        let mut copper = None;
        let mut state = cage_link(io, port.family, crosstalk)?;
        if let Some(w) = watch.as_mut() {
            let c = w.poll(io, port)?;
            // Spec 7.8.6: up only when LINKS and the X557 both say so.
            if !matches!(c, x552::Copper::Up { .. }) { state = Link::Down; }
            copper = Some(c);
        }
        let reforced = watch.as_ref().map_or(0, |w| w.reforced);
        if matches!(state, Link::Up { .. }) || elapsed >= millis {
            return Ok(Waited { link: state, copper, reforced });
        }
        delay_ms(io, 100);
        elapsed += 100;
    }
}

/// The 82599 link mode AUTOC/AUTOC2 select, for the console (AUTOC.LMS and
/// AUTOC2 10G PMA/PMD, spec 5.1, 5.2).
pub fn link_mode(autoc: u32, autoc2: u32) -> &'static str {
    match (autoc >> 13) & 7 {
        0 if autoc & (1 << 9) == 0 => "1G SFI",
        0 => "1G KX/BX, no AN",
        1 => match (autoc >> 7) & 3 {
            0 => "10G XAUI, no AN",
            1 => "10G KX4, no AN",
            2 => "10G CX4, no AN",
            _ => "10G parallel (reserved PMA/PMD)",
        },
        2 => "1G BX, clause 37 AN",
        3 => match (autoc2 >> 16) & 3 {
            0 => "10G KR, no AN",
            1 => "10G XFI",
            2 => "10G SFI",
            _ => "10G serial (reserved PMA/PMD)",
        },
        4 => "KX/KX4/KR AN",
        5 => "SGMII 100M/1G",
        6 => "KX/KX4/KR AN + 1G clause 37 AN",
        _ => "KX/KX4/KR AN + SGMII",
    }
}
