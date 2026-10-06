//! Software/firmware semaphores (spec 1.4).
//!
//! Two layers: a register semaphore guarding SW_FW_SYNC itself (82599:
//! SWSM.SMBI then SWSM.SWESMBI; X540 and later: SWSM.SMBI then
//! SW_FW_SYNC.REGSMP), then per-resource software and firmware bits in
//! SW_FW_SYNC. The X553 has its own SWSM and SW_FW_SYNC (spec 12.2), and a
//! third layer for MDIO: the firmware's PHY token (`TOKEN`, spec 12.5).
//!
//! Policy (spec 1.4.3 recommendation, section 10 item 15): a resource or
//! register semaphore still held after the bounded wait is never forced or
//! cleared; the access fails as `Error::Semaphore`, and the caller leaves the
//! PHY alone and reports link from LINKS only.

use super::{delay_ms, flush, hostif, read, write, Error, Family, Port, Registers, ESDP, R};

pub const EEP: u32 = 1 << 0;
pub const PHY0: u32 = 1 << 1;
pub const PHY1: u32 = 1 << 2;
pub const MAC_CSR: u32 = 1 << 3;
const FLASH: u32 = 1 << 4;
pub const SW_MNG: u32 = 1 << 10;
pub const I2C0: u32 = 1 << 11;
pub const I2C1: u32 = 1 << 12;
/// X552 SFP: both PHYs and both I2C buses, since the CS4227 and both cages
/// sit on one shared segment (spec 1.4.1, 7.7.1). Also the X553 SFP mask
/// (spec 12.11).
pub const SHARED_I2C: u32 = PHY0 | PHY1 | I2C0 | I2C1;
/// X553: the firmware's PHY token (spec 12.5). Software only: never written
/// to SW_FW_SYNC.
pub const TOKEN: u32 = 1 << 30;

const SWSM: u32 = 0x10140;
const SW_FW_SYNC: u32 = 0x10160;
const SWSM_X553: u32 = 0x15f70;
const SW_FW_SYNC_X553: u32 = 0x15f78;
/// PHY token: 5 s of 5 ms waits while the firmware answers "busy" (spec 12.5).
const TOKEN_TRIES: usize = 1000;
const TOKEN_DELAY_MS: usize = 5;
const SMBI: u32 = 1 << 0;
const SWESMBI: u32 = 1 << 1;
const REGSMP: u32 = 1 << 31;

/// The port's PHY semaphore bit.
pub fn phy(lan: u8) -> u32 { if lan == 1 { PHY1 } else { PHY0 } }

/// SWSM and SW_FW_SYNC for the family (spec 1.4, 12.2).
fn regs(family: Family) -> (u32, u32) {
    if family == Family::X553 { (SWSM_X553, SW_FW_SYNC_X553) } else { (SWSM, SW_FW_SYNC) }
}

/// SWSM.SMBI: 2000 reads 50 µs apart; the read that sees it clear grants it.
fn smbi<Io: Registers>(io: &mut Io, swsm: u32) -> R<(), Io::Error> {
    for _ in 0..2000 {
        if read(io, swsm)? & SMBI == 0 { return Ok(()); }
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
    let (swsm, sw_fw_sync) = regs(family);
    smbi(io, swsm)?;
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
        if read(io, sw_fw_sync)? & REGSMP == 0 { return Ok(()); }
        io.delay_us(50);
    }
    // Give back SMBI, which is ours; REGSMP is not, so it is left as it is.
    clear(io, swsm, SMBI)?;
    flush(io)?;
    Err(Error::Semaphore { held: REGSMP })
}

fn unlock<Io: Registers>(io: &mut Io, family: Family) -> R<(), Io::Error> {
    if family == Family::F82599 {
        clear(io, SWSM, SWESMBI | SMBI)?;
    } else {
        let (swsm, sw_fw_sync) = regs(family);
        clear(io, sw_fw_sync, REGSMP)?;
        clear(io, swsm, SMBI)?;
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

/// Acquire `mask`. With `TOKEN` (X553), the SW_FW_SYNC bits and then the
/// firmware's PHY token; while the firmware answers "busy" the bits are
/// given back, and both are asked for again 5 ms later, for up to 5 s
/// (spec 12.5).
pub fn acquire<Io: Registers>(io: &mut Io, port: Port, mask: u32) -> R<(), Io::Error> {
    let hw = mask & !TOKEN;
    if mask & TOKEN == 0 { return acquire_bits(io, port, hw); }
    for _ in 0..TOKEN_TRIES {
        if hw != 0 { acquire_bits(io, port, hw)?; }
        let granted = hostif::token(io, port, true);
        if let Ok(true) = granted { return Ok(()); }
        if hw != 0 { release_bits(io, port, hw)?; }
        granted?;
        delay_ms(io, TOKEN_DELAY_MS);
    }
    Err(Error::Firmware { command: hostif::TOKEN_COMMAND, status: hostif::TOKEN_BUSY })
}

/// Release `mask`: the PHY token first (X553), then the SW_FW_SYNC bits,
/// which are released even if the token release failed.
pub fn release<Io: Registers>(io: &mut Io, port: Port, mask: u32) -> R<(), Io::Error> {
    let hw = mask & !TOKEN;
    let token = if mask & TOKEN != 0 { hostif::token(io, port, false).map(|_| ()) } else { Ok(()) };
    if hw != 0 { release_bits(io, port, hw)?; }
    token
}

/// Acquire `mask` in SW_FW_SYNC: 200 attempts 5 ms apart (82599, X540),
/// 1000 from the X550 on (spec 1.4.2, 1.4.3, 12.2). On the X552 a mask with
/// an I2C bit also switches port 1's I2C mux (spec 7.7.1).
fn acquire_bits<Io: Registers>(io: &mut Io, port: Port, mask: u32) -> R<(), Io::Error> {
    let (sw, fw, hw) = bits(port.family, mask);
    let (_, sw_fw_sync) = regs(port.family);
    let attempts = if matches!(port.family, Family::F82599 | Family::X540) { 200 } else { 1000 };
    let mut held = 0;
    for _ in 0..attempts {
        lock(io, port.family)?;
        let sync = match read(io, sw_fw_sync) {
            Ok(v) => v,
            Err(e) => { unlock(io, port.family)?; return Err(e); }
        };
        held = sync & (sw | fw | hw);
        if held == 0 {
            let taken = write(io, sw_fw_sync, sync | sw);
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
/// PHY/MNG masks and 2 ms for others from the X540 on (spec 1.4.3).
fn release_bits<Io: Registers>(io: &mut Io, port: Port, mask: u32) -> R<(), Io::Error> {
    if mux(port, mask) { set_mux(io, false)?; }
    let (sw, _, _) = bits(port.family, mask);
    let (_, sw_fw_sync) = regs(port.family);
    lock(io, port.family)?;
    let cleared = clear(io, sw_fw_sync, sw);
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
/// The X553 has no mux (spec 12.11).
fn mux(port: Port, mask: u32) -> bool {
    port.family == Family::X552 && port.lan == 1 && mask & (I2C0 | I2C1) != 0
}

fn set_mux<Io: Registers>(io: &mut Io, on: bool) -> R<(), Io::Error> {
    let esdp = read(io, ESDP)?;
    write(io, ESDP, if on { esdp | (1 << 1) } else { esdp & !(1 << 1) })?;
    flush(io)
}
