//! Clause 45 MDIO through MSCA/MSRWD (spec 2), under the port's PHY
//! semaphore, taken and released per register (spec 2.2); on the X553 also
//! under the firmware's PHY token (spec 12.5).

use super::{delay_ms, read, sync, write, Error, Family, Port, Registers, R};

const MSCA: u32 = 0x0425c;
const MSRWD: u32 = 0x04260;
const COMMAND: u32 = 1 << 30;
const OP_WRITE: u32 = 1 << 26;
const OP_READ: u32 = 3 << 26;

pub const PMA: u8 = 1;
pub const PHY_XS: u8 = 4;
pub const AN: u8 = 7;
pub const VENDOR: u8 = 0x1e;

/// Start a cycle and poll MSCA every 10 µs, 100 times, for completion.
fn cycle<Io: Registers>(io: &mut Io, command: u32) -> R<(), Io::Error> {
    write(io, MSCA, command | COMMAND)?;
    let mut last = 0;
    for _ in 0..100 {
        io.delay_us(10);
        last = read(io, MSCA)?;
        if last & COMMAND == 0 { return Ok(()); }
    }
    Err(Error::Timeout { register: MSCA, mask: COMMAND, expected: 0, last })
}

/// What an MDIO access holds: the port's PHY bit, plus the token on the X553.
fn mask(port: Port) -> u32 {
    sync::phy(port.lan) | if port.family == Family::X553 { sync::TOKEN } else { 0 }
}

fn address(phy: u8, dev: u8, reg: u16) -> u32 {
    reg as u32 | (dev as u32) << 16 | (phy as u32) << 21
}

/// `dev.reg` of the PHY at `phy`: address cycle, then read cycle.
pub fn read_reg<Io: Registers>(io: &mut Io, port: Port, phy: u8, dev: u8, reg: u16) -> R<u16, Io::Error> {
    sync::locked(io, port, mask(port), |io| {
        let a = address(phy, dev, reg);
        cycle(io, a)?;
        cycle(io, a | OP_READ)?;
        Ok((io.read(MSRWD).map_err(Error::Io)? >> 16) as u16)
    })
}

pub fn write_reg<Io: Registers>(io: &mut Io, port: Port, phy: u8, dev: u8, reg: u16, value: u16) -> R<(), Io::Error> {
    sync::locked(io, port, mask(port), |io| {
        write(io, MSRWD, value as u32)?;
        let a = address(phy, dev, reg);
        cycle(io, a)?;
        cycle(io, a | OP_WRITE)
    })
}

/// Read-modify-write; each access holds the semaphore on its own.
pub fn modify<Io: Registers>(io: &mut Io, port: Port, phy: u8, dev: u8, reg: u16,
    f: impl FnOnce(u16) -> u16) -> R<u16, Io::Error> {
    let value = f(read_reg(io, port, phy, dev, reg)?);
    write_reg(io, port, phy, dev, reg, value)?;
    Ok(value)
}

/// A PHY answers at `phy` if 1.0x0002 is neither 0 nor all ones (spec 2.5).
/// The ID is 1.0x0002 << 16 | 1.0x0003 with the revision nibble masked.
pub fn probe<Io: Registers>(io: &mut Io, port: Port, phy: u8) -> R<Option<u32>, Io::Error> {
    let high = read_reg(io, port, phy, PMA, 2)?;
    if high == 0 || high == 0xffff { return Ok(None); }
    let low = read_reg(io, port, phy, PMA, 3)?;
    Ok(Some((high as u32) << 16 | (low & 0xfff0) as u32))
}

/// Addresses 0 to 31 in order; the first valid one wins (spec 2.5).
pub fn scan<Io: Registers>(io: &mut Io, port: Port) -> R<Option<(u8, u32)>, Io::Error> {
    for phy in 0..32 {
        if let Some(id) = probe(io, port, phy)? { return Ok(Some((phy, id))); }
    }
    Ok(None)
}

/// The PHY IDs of spec 2.5 this driver meets, for the console.
pub fn name(id: u32) -> &'static str {
    match id {
        0x0154_0200 => "X540",
        0x0154_0220 => "X550",
        0x0154_0240 | 0x0154_0250 => "X557",
        0x00a1_9410 => "TN1010",
        0x0141_0dd0 => "88E1500",
        0x0141_0ea0 => "88E1543",
        _ => "unknown PHY",
    }
}

/// Generic PHY soft reset (spec 2.6): 4.0x0000 bit 15, then up to 30 polls
/// 100 ms apart; the X557 signals completion in 1.0xCC02 bits 1:0, others by
/// clearing bit 15. The caller checks the manageability veto first.
pub fn reset<Io: Registers>(io: &mut Io, port: Port, phy: u8, x557: bool) -> R<(), Io::Error> {
    write_reg(io, port, phy, PHY_XS, 0, 0x8000)?;
    for _ in 0..30 {
        delay_ms(io, 100);
        let done = if x557 {
            read_reg(io, port, phy, PMA, 0xcc02)? & 3 != 0
        } else {
            read_reg(io, port, phy, PHY_XS, 0)? & 0x8000 == 0
        };
        if done { io.delay_us(2); break; }
    }
    if read_reg(io, port, phy, PHY_XS, 0)? & 0x8000 != 0 { return Err(Error::PhyReset); }
    Ok(())
}

/// A set of link speeds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Speeds { pub g10: bool, pub g1: bool, pub m100: bool }

pub const TEN: Speeds = Speeds { g10: true, g1: false, m100: false };
pub const ONE: Speeds = Speeds { g10: false, g1: true, m100: false };

/// Copper speeds from 1.0x0004: bit 0 10G, bit 4 1G, bit 5 100M (spec 6.4).
pub fn abilities<Io: Registers>(io: &mut Io, port: Port, phy: u8) -> R<Speeds, Io::Error> {
    let v = read_reg(io, port, phy, PMA, 4)?;
    Ok(Speeds { g10: v & 1 != 0, g1: v & 0x10 != 0, m100: v & 0x20 != 0 })
}

fn bit(value: u16, mask: u16, on: bool) -> u16 { if on { value | mask } else { value & !mask } }

/// Advertise `s` and restart AN unless vetoed (spec 6.5; X540, X550 and
/// X557): 7.0x0020 bit 12 (10G), 7.0xC400 bit 15 (1G), 7.0x0010 bit 7
/// cleared and bit 8 (100M), then 7.0x0000 bit 9. `nbase`: the X550's 5G
/// and 2.5G (7.0xC400 bits 11 and 10, spec 12.3); None leaves them as they
/// are. Returns whether AN was restarted.
pub fn advertise<Io: Registers>(io: &mut Io, port: Port, phy: u8, s: Speeds, nbase: Option<bool>,
    veto: bool) -> R<bool, Io::Error> {
    modify(io, port, phy, AN, 0x0020, |v| bit(v, 1 << 12, s.g10))?;
    modify(io, port, phy, AN, 0xc400, |v| {
        let v = match nbase { Some(on) => bit(v, (1 << 11) | (1 << 10), on), None => v };
        bit(v, 1 << 15, s.g1)
    })?;
    modify(io, port, phy, AN, 0x0010, |v| bit(v & !(1 << 7), 1 << 8, s.m100))?;
    if veto { return Ok(false); }
    modify(io, port, phy, AN, 0x0000, |v| v | (1 << 9))?;
    Ok(true)
}

/// Copper link, 7.0x0001 bit 2. It latches low: the second of two
/// back-to-back reads is the current state (spec 6.6).
pub fn an_link<Io: Registers>(io: &mut Io, port: Port, phy: u8) -> R<bool, Io::Error> {
    read_reg(io, port, phy, AN, 1)?;
    Ok(read_reg(io, port, phy, AN, 1)? & (1 << 2) != 0)
}
