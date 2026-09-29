//! Software/firmware semaphores (spec 1.4).
//!
//! Two layers: a register semaphore guarding SW_FW_SYNC itself (82599:
//! SWSM.SMBI then SWSM.SWESMBI; X540/X552: SWSM.SMBI then SW_FW_SYNC.REGSMP),
//! then per-resource software and firmware bits in SW_FW_SYNC.
//!
//! Policy (spec 1.4.3 recommendation, section 10 item 15): a resource or
//! register semaphore still held after the bounded wait is never forced or
//! cleared; the access fails as `Error::Semaphore`, and the caller leaves the
//! PHY alone and reports link from LINKS only.

use super::{delay_ms, flush, read, write, Error, Family, Port, Registers, ESDP, R};

pub const EEP: u32 = 1 << 0;
pub const PHY0: u32 = 1 << 1;
pub const PHY1: u32 = 1 << 2;
pub const MAC_CSR: u32 = 1 << 3;
const FLASH: u32 = 1 << 4;
pub const SW_MNG: u32 = 1 << 10;
pub const I2C0: u32 = 1 << 11;
pub const I2C1: u32 = 1 << 12;
/// X552 SFP: both PHYs and both I2C buses, since the CS4227 and both cages
/// sit on one shared segment (spec 1.4.1, 7.7.1).
pub const SHARED_I2C: u32 = PHY0 | PHY1 | I2C0 | I2C1;

const SWSM: u32 = 0x10140;
const SW_FW_SYNC: u32 = 0x10160;
const SMBI: u32 = 1 << 0;
const SWESMBI: u32 = 1 << 1;
const REGSMP: u32 = 1 << 31;

/// The port's PHY semaphore bit.
pub fn phy(lan: u8) -> u32 { if lan == 1 { PHY1 } else { PHY0 } }

/// SWSM.SMBI: 2000 reads 50 µs apart; the read that sees it clear grants it.
fn smbi<Io: Registers>(io: &mut Io) -> R<(), Io::Error> {
    for _ in 0..2000 {
        if read(io, SWSM)? & SMBI == 0 { return Ok(()); }
        io.delay_us(50);
    }
    Err(Error::Semaphore { held: SMBI })
}

fn clear<Io: Registers>(io: &mut Io, reg: u32, bits: u32) -> R<(), Io::Error> {
    let value = read(io, reg)?;
    write(io, reg, value & !bits)
}

/// The register semaphore (spec 1.4.2 steps 1–2, 1.4.3).
fn lock<Io: Registers>(io: &mut Io, family: Family) -> R<(), Io::Error> {
    smbi(io)?;
    if family == Family::F82599 {
        for _ in 0..2000 {
            let swsm = read(io, SWSM)?;
            write(io, SWSM, swsm | SWESMBI)?;
            if read(io, SWSM)? & SWESMBI != 0 { return Ok(()); }
            io.delay_us(50);
        }
        clear(io, SWSM, SWESMBI | SMBI)?;
        flush(io)?;
        return Err(Error::Semaphore { held: SWESMBI });
    }
    for _ in 0..2000 {
        if read(io, SW_FW_SYNC)? & REGSMP == 0 { return Ok(()); }
        io.delay_us(50);
    }
    // Give back SMBI, which is ours; REGSMP is not, so it is left as it is.
    clear(io, SWSM, SMBI)?;
    flush(io)?;
    Err(Error::Semaphore { held: REGSMP })
}

fn unlock<Io: Registers>(io: &mut Io, family: Family) -> R<(), Io::Error> {
    if family == Family::F82599 {
        clear(io, SWSM, SWESMBI | SMBI)?;
    } else {
        clear(io, SW_FW_SYNC, REGSMP)?;
        clear(io, SWSM, SMBI)?;
    }
    flush(io)
}

/// Software bits taken, and the firmware and hardware bits that block them.
fn bits(family: Family, mask: u32) -> (u32, u32, u32) {
    if family == Family::F82599 {
        let sw = mask & 0x1f;
        return (sw, sw << 5, 0);
    }
    let i2c = mask & (I2C0 | I2C1);
    let sw = mask & (0xf | SW_MNG) | i2c;
    let fw = (mask & 0xf) << 5 | i2c << 2;
    let hw = if mask & EEP != 0 { FLASH } else { 0 };
    (sw, fw, hw)
}

/// Acquire `mask` in SW_FW_SYNC: 200 attempts 5 ms apart (82599, X540),
/// 1000 on the X552 (spec 1.4.2, 1.4.3). On the X552 a mask with an I2C bit
/// also switches port 1's I2C mux (spec 7.7.1).
pub fn acquire<Io: Registers>(io: &mut Io, port: Port, mask: u32) -> R<(), Io::Error> {
    let (sw, fw, hw) = bits(port.family, mask);
    let attempts = if port.family == Family::X552 { 1000 } else { 200 };
    let mut held = 0;
    for _ in 0..attempts {
        lock(io, port.family)?;
        let sync = match read(io, SW_FW_SYNC) {
            Ok(v) => v,
            Err(e) => { unlock(io, port.family)?; return Err(e); }
        };
        held = sync & (sw | fw | hw);
        if held == 0 {
            let taken = write(io, SW_FW_SYNC, sync | sw);
            unlock(io, port.family)?;
            taken?;
            if mux(port, mask) { set_mux(io, true)?; }
            return Ok(());
        }
        unlock(io, port.family)?;
        delay_ms(io, 5);
    }
    Err(Error::Semaphore { held })
}

/// Release `mask`: the mux first, then the software bits; then 10 µs for
/// PHY/MNG masks and 2 ms for others on the X540/X552 (spec 1.4.3).
pub fn release<Io: Registers>(io: &mut Io, port: Port, mask: u32) -> R<(), Io::Error> {
    if mux(port, mask) { set_mux(io, false)?; }
    let (sw, _, _) = bits(port.family, mask);
    lock(io, port.family)?;
    let cleared = clear(io, SW_FW_SYNC, sw);
    unlock(io, port.family)?;
    cleared?;
    if port.family != Family::F82599 {
        io.delay_us(if mask & (PHY0 | PHY1 | SW_MNG) != 0 { 10 } else { 2000 });
    }
    Ok(())
}

/// Run `f` holding `mask`; release it whether or not `f` succeeded.
pub fn locked<Io: Registers, T>(io: &mut Io, port: Port, mask: u32,
    f: impl FnOnce(&mut Io) -> R<T, Io::Error>) -> R<T, Io::Error> {
    acquire(io, port, mask)?;
    let result = f(io);
    let released = release(io, port, mask);
    let value = result?;
    released?;
    Ok(value)
}

/// X552 port 1 reaches the shared I2C segment through a mux on its SDP1.
fn mux(port: Port, mask: u32) -> bool {
    port.family == Family::X552 && port.lan == 1 && mask & (I2C0 | I2C1) != 0
}

fn set_mux<Io: Registers>(io: &mut Io, on: bool) -> R<(), Io::Error> {
    let esdp = read(io, ESDP)?;
    write(io, ESDP, if on { esdp | (1 << 1) } else { esdp & !(1 << 1) })?;
    flush(io)
}
