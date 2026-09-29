//! Bring-up primitives from the datasheets (X552 PHY setup from Intel's
//! BSD-licensed shared code, `x552`); see docs/bring-up.md for provenance.
//!
//! Kept independent of UEFI so failure sequences can be tested without a NIC.
//! The caller owns PciIo BY_DRIVER, enables BAR memory access, and supplies
//! firmware Stall for delays. No DMA buffers or bus-master enable are needed.

pub const CTRL: u32 = 0x00000;
pub const STATUS: u32 = 0x00008;
pub const LINKS: u32 = 0x042a4;
pub const AUTOC: u32 = 0x042a0;
pub const AUTOC2: u32 = 0x042a8;
pub const ESDP: u32 = 0x00020;
const RXCTRL: u32 = 0x03000;
const EIMC: u32 = 0x00888;
const EEC: u32 = 0x10010;
const EEMNGCTL: u32 = 0x10110;
const RDRXCTL: u32 = 0x02f00;
const RAL0: u32 = 0x0a200;
const RAH0: u32 = 0x0a204;
const MASTER_DISABLE: u32 = 1 << 2;
const MASTER_ENABLED: u32 = 1 << 19;
const RESET: u32 = (1 << 26) | (1 << 3);
const QUEUE_ENABLE: u32 = 1 << 25;
const RESTART_AN: u32 = 1 << 12;

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
    /// SW_FW_SYNC resource (or SWSM.SMBI / REGSMP) still held by firmware or
    /// another driver after the bounded wait; never taken from its owner.
    Semaphore { held: u32 },
    /// IOSF sideband access to the X552 KR PHY returned an error response.
    Sideband { address: u32, ctrl: u32 },
    /// No ACK from I2C device `device` (8-bit address) after retries.
    I2c { device: u8 },
    /// CS4227 reset did not load its image (register and last value read).
    Cs4227 { register: u16, value: u16 },
    /// No external PHY answered on MDIO.
    NoPhy,
}

#[path = "x552.rs"]
pub mod x552;

fn read<R: Registers>(io: &mut R, reg: u32) -> Result<u32, Error<R::Error>> {
    let value = io.read(reg).map_err(Error::Io)?;
    // These status/control registers cannot legitimately have all bits set.
    // Do not interpret a disappeared PCI function as reset/NVM/link success.
    if value == u32::MAX { return Err(Error::Removed); }
    Ok(value)
}

fn write<R: Registers>(io: &mut R, reg: u32, value: u32) -> Result<(), Error<R::Error>> {
    io.write(reg, value).map_err(Error::Io)
}

/// Poll at 1 ms intervals, including a last read at the deadline.
fn wait<R: Registers>(io: &mut R, reg: u32, mask: u32, expected: u32,
    millis: usize) -> Result<u32, Error<R::Error>> {
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

fn mask_interrupts<R: Registers>(io: &mut R) -> Result<(), Error<R::Error>> {
    write(io, EIMC, 0x7fff_ffff)?;
    write(io, 0x00ab0, u32::MAX)?;
    write(io, 0x00ab4, u32::MAX)
}

fn rxdctl(queue: u32) -> u32 {
    if queue < 64 { 0x01028 + queue * 0x40 }
    else { 0x0d028 + (queue - 64) * 0x40 }
}

/// Stop host reception and drain outstanding PCIe requests before reset.
/// A drain timeout fails closed; no forced reset or stealing another owner's
/// semaphore is attempted. Interrupts and reception remain disabled on error.
pub fn quiesce<R: Registers>(io: &mut R) -> Result<(), Error<R::Error>> {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Identity {
    pub lan: u8,
    /// NVM-provisioned address reloaded into RAR0 by this reset. Never taken
    /// from RAR0 before reset, where an earlier driver may have replaced it.
    pub mac: [u8; 6],
}

/// Global software + link reset, NVM completion, and per-port MAC retrieval.
/// Leaves all host interrupts masked and RX/TX disabled by reset. Does not
/// configure the PHY or claim a network interface is ready.
pub fn reset<R: Registers>(io: &mut R) -> Result<Identity, Error<R::Error>> {
    let lan = ((read(io, STATUS)? >> 2) & 3) as u8;
    if lan > 1 { return Err(Error::InvalidPort); }
    quiesce(io)?;
    let ctrl = read(io, CTRL)?;
    write(io, CTRL, ctrl | RESET)?;
    // Datasheet forbids even a flush read in the first millisecond.
    io.delay_us(1000);
    wait(io, CTRL, RESET, 0, 100)?;
    io.delay_us(10_000);
    mask_interrupts(io)?;
    let eec = wait(io, EEC, 1 << 9, 1 << 9, 1000)?;
    // AUTO_RD also sets for absent or invalid NVM; require EE_PRES as well.
    if eec & (1 << 8) == 0 { return Err(Error::MissingNvm); }
    let cfg = 1 << (18 + lan);
    wait(io, EEMNGCTL, cfg, cfg, 1000)?;
    wait(io, RDRXCTL, 1 << 3, 1 << 3, 1000)?;
    // Unlike control/status registers, RAL can legitimately be all ones.
    let low = io.read(RAL0).map_err(Error::Io)?.to_le_bytes();
    let high = read(io, RAH0)?;
    let mac = [low[0], low[1], low[2], low[3], high as u8, (high >> 8) as u8];
    if high & (1 << 31) == 0 || mac == [0; 6] || mac[0] & 1 != 0 {
        return Err(Error::InvalidMac);
    }
    Ok(Identity { lan, mac })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Link {
    Down,
    /// None means link is up but the speed encoding is reserved, not 0 Mb/s.
    Up { megabits: Option<u32> },
}

/// Read the current LINK_UP bit, not the latched historical LINK_STATUS bit.
/// A cable unplugged is a normal state, not a bring-up timeout.
pub fn link<R: Registers>(io: &mut R) -> Result<Link, Error<R::Error>> {
    let value = read(io, LINKS)?;
    if value & (1 << 30) == 0 { return Ok(Link::Down); }
    let megabits = match (value >> 28) & 3 {
        1 => Some(100), 2 => Some(1000), 3 => Some(10_000), _ => None,
    };
    Ok(Link::Up { megabits })
}

/// Controller family; the link setup differs, the reset above does not
/// (EEC, EEMNGCTL, RDRXCTL, RAR0 and LINKS share offsets in all three).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family { F82599, X540, X552 }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Setup {
    /// 82599: the NVM-loaded AUTOC/AUTOC2 were applied with Restart_AN.
    /// ESDP is read only for the console: SFP+ TX_DISABLE/MOD_ABS pins.
    Restarted { autoc: u32, autoc2: u32, esdp: u32 },
    /// X540: the integrated PHY negotiates from its NVM image; nothing written.
    PhyAutonomous,
    /// X552: per-device PHY setup (`x552::setup`).
    X552(x552::Setup),
}

/// Link setup after `reset`, per family.
///
/// 82599 (datasheet 4.6.3.2, 4.6.4, 3.7.4.4): the link interconnect and link
/// mode (AUTOC.LMS and PMA/PMD fields, AUTOC2.10G_PMA_PMD_Serial) are loaded
/// from the NVM, which describes the board's media; software applies them
/// with AUTOC.Restart_AN. They are not rewritten here: the NVM is the only
/// source of what the board wires to the MAC. SDPs (SFP+ TX_DISABLE and
/// module presence) are board-specific (Table 3-13 is an example) and
/// preserved across resets, so they are left as firmware set them.
///
/// X540 (datasheet 3.6.3.2, 4.6.2): the NVM holds enough to bring the link
/// up; the PHY auto-negotiates by itself and software only changes its
/// settings to depart from the defaults. No MDIO write is made.
///
/// X552: depends on the PHY behind the MAC, so on the device ID; see `x552`.
pub fn setup_link<R: Registers>(io: &mut R, family: Family, device: u16, lan: u8)
    -> Result<Setup, Error<R::Error>> {
    match family {
        Family::F82599 => {
            let autoc = read(io, AUTOC)?;
            let autoc2 = read(io, AUTOC2)?;
            let esdp = read(io, ESDP)?;
            write(io, AUTOC, autoc | RESTART_AN)?;
            Ok(Setup::Restarted { autoc, autoc2, esdp })
        }
        Family::X540 => Ok(Setup::PhyAutonomous),
        Family::X552 => Ok(Setup::X552(x552::setup(io, device, lan)?)),
    }
}

/// The 82599 link mode AUTOC/AUTOC2 select, for the console (AUTOC.LMS,
/// datasheet 8.2.3.22.19; 10G_PMA_PMD_Serial, 8.2.3.22.22).
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
            2 => "10G SFI",
            _ => "10G serial (reserved PMA/PMD)",
        },
        4 => "KX/KX4/KR AN",
        5 => "SGMII 100M/1G",
        6 => "KX/KX4/KR AN + 1G clause 37 AN",
        _ => "KX/KX4/KR AN + SGMII",
    }
}

/// Poll `link` every 10 ms for up to `millis`; link down at the end is a
/// result, not an error (no cable, or the partner is still negotiating).
pub fn wait_link<R: Registers>(io: &mut R, millis: usize) -> Result<Link, Error<R::Error>> {
    let mut elapsed = 0;
    loop {
        let state = link(io)?;
        if matches!(state, Link::Up { .. }) || elapsed >= millis { return Ok(state); }
        io.delay_us(10_000);
        elapsed += 10;
    }
}
