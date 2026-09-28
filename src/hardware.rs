//! Datasheet-only bring-up primitives; see docs/bring-up.md for provenance.
//!
//! Kept independent of UEFI so failure sequences can be tested without a NIC.
//! The caller owns PciIo BY_DRIVER, enables BAR memory access, and supplies
//! firmware Stall for delays. No DMA buffers or bus-master enable are needed.

pub const CTRL: u32 = 0x00000;
pub const STATUS: u32 = 0x00008;
pub const LINKS: u32 = 0x042a4;
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
}

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
