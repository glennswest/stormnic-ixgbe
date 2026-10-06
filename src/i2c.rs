//! Bit-banged I2C through I2CCTL (spec 3): the SFP+ ID EEPROM, the X552
//! port expander and the CS4227 retimer.
//!
//! Two layouts (spec 3.1): the 82599/X540 I2CCTL at 0x28 releases a line by
//! writing its "out" bit as 1; the X552/X553 I2CCTL at 0x15F5C also has
//! active-low output enables and a bit-bang enable (spec 12.2).
//!
//! Timing (spec 3.2), in µs: rise + fall + set-up 3, clock high 4, low 5,
//! start set-up 5, start hold 4, stop set-up 4, bus free 5; SCL waits up to
//! 500 µs for a stretching slave; ACK is sampled 10 times 1 µs apart.

use super::{delay_ms, read, sync, write, Error, Family, Port, Registers, ESDP, R, STATUS};

#[derive(Clone, Copy)]
struct Layout {
    reg: u32,
    scl_in: u32, scl_out: u32, sda_in: u32, sda_out: u32,
    /// X552/X553 only (0 on the 82599/X540).
    sda_oe_n: u32, scl_oe_n: u32, bb_en: u32,
}

const L82599: Layout = Layout {
    reg: 0x00028, scl_in: 1 << 0, scl_out: 1 << 1, sda_in: 1 << 2, sda_out: 1 << 3,
    sda_oe_n: 0, scl_oe_n: 0, bb_en: 0,
};
const LX552: Layout = Layout {
    reg: 0x15f5c, scl_in: 1 << 14, scl_out: 1 << 9, sda_in: 1 << 12, sda_out: 1 << 10,
    sda_oe_n: 1 << 11, scl_oe_n: 1 << 13, bb_en: 1 << 8,
};

struct Bus<'a, Io> { io: &'a mut Io, l: Layout, ctl: u32 }

impl<'a, Io: Registers> Bus<'a, Io> {
    fn new(io: &'a mut Io, family: Family) -> R<Self, Io::Error> {
        let l = if x550em(family) { LX552 } else { L82599 };
        let ctl = io.read(l.reg).map_err(Error::Io)?;
        Ok(Bus { io, l, ctl })
    }
    /// Write I2CCTL and flush.
    fn put(&mut self) -> R<(), Io::Error> {
        self.io.write(self.l.reg, self.ctl).map_err(Error::Io)?;
        self.io.read(STATUS).map(|_| ()).map_err(Error::Io)
    }
    fn get(&mut self) -> R<u32, Io::Error> { self.io.read(self.l.reg).map_err(Error::Io) }
    fn delay(&mut self, us: usize) { self.io.delay_us(us); }

    fn scl_high(&mut self) -> R<(), Io::Error> {
        self.ctl |= self.l.scl_oe_n;
        for _ in 0..500 {
            self.ctl |= self.l.scl_out;
            self.put()?;
            self.delay(1);
            if self.get()? & self.l.scl_in != 0 { break; }
        }
        Ok(())
    }
    fn scl_low(&mut self) -> R<(), Io::Error> {
        self.ctl &= !(self.l.scl_out | self.l.scl_oe_n);
        self.put()?;
        self.delay(1);
        Ok(())
    }
    /// Drive SDA low, or release it high and check nothing holds it low.
    fn sda(&mut self, high: bool) -> R<(), Io::Error> {
        if high { self.ctl |= self.l.sda_out } else { self.ctl &= !self.l.sda_out }
        self.ctl &= !self.l.sda_oe_n;
        self.put()?;
        self.delay(3);
        if !high { return Ok(()); }
        if self.l.sda_oe_n != 0 {
            self.ctl |= self.l.sda_oe_n;
            self.put()?;
        }
        if self.get()? & self.l.sda_in == 0 { return Err(Error::I2c { device: 0 }); }
        Ok(())
    }
    /// Let the slave drive SDA: out bit 1 (open drain), X552 output enable off.
    fn release_sda(&mut self) -> R<(), Io::Error> {
        self.ctl |= self.l.sda_out | self.l.sda_oe_n;
        self.put()?;
        self.delay(1);
        Ok(())
    }
    fn start(&mut self) -> R<(), Io::Error> {
        self.ctl = self.get()? | self.l.bb_en;
        self.sda(true)?;
        self.scl_high()?;
        self.delay(5);
        self.sda(false)?;
        self.delay(4);
        self.scl_low()?;
        self.delay(5);
        Ok(())
    }
    fn stop(&mut self) -> R<(), Io::Error> {
        self.sda(false)?;
        self.scl_high()?;
        self.delay(4);
        self.sda(true)?;
        self.delay(5);
        if self.l.bb_en != 0 {
            self.ctl &= !self.l.bb_en;
            self.ctl |= self.l.sda_oe_n | self.l.scl_oe_n;
            self.put()?;
        }
        Ok(())
    }
    fn out_bit(&mut self, bit: bool) -> R<(), Io::Error> {
        self.sda(bit)?;
        self.scl_high()?;
        self.delay(4);
        self.scl_low()?;
        self.delay(5);
        Ok(())
    }
    fn in_bit(&mut self) -> R<bool, Io::Error> {
        self.release_sda()?;
        self.scl_high()?;
        self.delay(4);
        let bit = self.get()? & self.l.sda_in != 0;
        self.scl_low()?;
        self.delay(5);
        Ok(bit)
    }
    fn in_byte(&mut self) -> R<u8, Io::Error> {
        let mut byte = 0;
        for _ in 0..8 { byte = byte << 1 | self.in_bit()? as u8; }
        Ok(byte)
    }
    /// Eight bits, MSB first, then SDA released; then the slave's ACK: SDA
    /// low within 10 samples 1 µs apart while SCL is high.
    fn send(&mut self, device: u8, byte: u8) -> R<(), Io::Error> {
        for i in (0..8).rev() { self.out_bit(byte >> i & 1 != 0)?; }
        self.release_sda()?;
        self.scl_high()?;
        self.delay(4);
        let mut acked = false;
        for _ in 0..10 {
            if self.get()? & self.l.sda_in == 0 { acked = true; break; }
            self.delay(1);
        }
        self.scl_low()?;
        self.delay(5);
        if !acked { return Err(Error::I2c { device }); }
        Ok(())
    }
    /// After a failed transfer: start, SDA released, nine SCL pulses, start,
    /// stop, to free a slave stuck mid-byte.
    fn clear(&mut self) -> R<(), Io::Error> {
        let quiet = |r: R<(), Io::Error>| match r { Err(Error::I2c { .. }) => Ok(()), other => other };
        quiet(self.start())?;
        self.release_sda()?;
        for _ in 0..9 {
            self.scl_high()?;
            self.delay(4);
            self.scl_low()?;
            self.delay(5);
        }
        quiet(self.start())?;
        quiet(self.stop())
    }
}

/// The X552 and X553 (the shared code's X550EM): the X550 I2CCTL layout,
/// the shared-segment semaphore, fewer read attempts.
fn x550em(family: Family) -> bool { matches!(family, Family::X552 | Family::X553) }

/// The semaphore guarding the port's I2C bus (spec 3.4): the port's PHY bit
/// on the 82599, the shared-segment mask on the X552 (with the mux) and the
/// X553 (spec 12.11).
fn mask(port: Port) -> u32 {
    if x550em(port.family) { sync::SHARED_I2C } else { sync::phy(port.lan) }
}

/// Read attempts for a byte other than the probe of the identifier (spec
/// 3.3): 11 on the 82599/X540, 4 on the X552/X553.
pub fn attempts(port: Port) -> usize { if x550em(port.family) { 4 } else { 11 } }

/// One transfer; on an I2C failure the bus is cleared.
fn attempt<Io: Registers, T>(io: &mut Io, port: Port, device: u8,
    f: &mut impl FnMut(&mut Bus<'_, Io>) -> R<T, Io::Error>) -> R<T, Io::Error> {
    let mut bus = Bus::new(io, port.family)?;
    match f(&mut bus) {
        Err(Error::I2c { .. }) => {
            match bus.clear() { Ok(()) | Err(Error::I2c { .. }) => {}, Err(e) => return Err(e) }
            Err(Error::I2c { device })
        }
        other => other,
    }
}

/// 82599 QSFP (0x1558): the I2C bus is shared by the ports; request it on
/// SDP0 and wait for the grant on SDP1, up to 200 × 5 ms (spec 3.5).
fn qsfp(port: Port) -> bool { port.family == Family::F82599 && port.device == 0x1558 }

/// ESDP set-up for the QSFP handshake, once at PHY init (spec 3.5).
pub fn qsfp_setup<Io: Registers>(io: &mut Io) -> R<(), Io::Error> {
    let esdp = read(io, ESDP)?;
    let esdp = (esdp | (1 << 8)) & !((1 << 9) | 1 | (1 << 16) | (1 << 17));
    write(io, ESDP, esdp)?;
    super::flush(io)
}

fn with_bus<Io: Registers, T>(io: &mut Io, port: Port, device: u8,
    f: impl FnOnce(&mut Io) -> R<T, Io::Error>) -> R<T, Io::Error> {
    if !qsfp(port) { return f(io); }
    let esdp = read(io, ESDP)?;
    write(io, ESDP, esdp | 1)?;
    super::flush(io)?;
    let mut granted = false;
    for _ in 0..200 {
        if read(io, ESDP)? & (1 << 1) != 0 { granted = true; break; }
        delay_ms(io, 5);
    }
    let result = if granted { f(io) } else { Err(Error::I2c { device }) };
    let esdp = read(io, ESDP)?;
    write(io, ESDP, esdp & !1)?;
    super::flush(io)?;
    result
}

/// Read byte `offset` of `device` (spec 3.3): start, address, offset,
/// repeated start, address | 1, eight bits, NACK, stop. With `lock`, the
/// semaphore is taken per attempt and a failed attempt waits 100 ms.
pub fn read_byte<Io: Registers>(io: &mut Io, port: Port, device: u8, offset: u8,
    attempts: usize, lock: bool) -> R<u8, Io::Error> {
    with_bus(io, port, device, |io| {
        for _ in 0..attempts {
            if lock { sync::acquire(io, port, mask(port))?; }
            let result = attempt(io, port, device, &mut |bus| {
                bus.start()?;
                bus.send(device, device)?;
                bus.send(device, offset)?;
                bus.start()?;
                bus.send(device, device | 1)?;
                let value = bus.in_byte()?;
                bus.out_bit(true)?;
                bus.stop()?;
                Ok(value)
            });
            if lock { sync::release(io, port, mask(port))?; }
            match result {
                Err(Error::I2c { .. }) => if lock { delay_ms(io, 100) },
                other => return other,
            }
        }
        Err(Error::I2c { device })
    })
}

/// Write a byte: start, address, offset, data, stop; 2 attempts, the
/// semaphore held across both (spec 3.3).
pub fn write_byte<Io: Registers>(io: &mut Io, port: Port, device: u8, offset: u8, value: u8,
    lock: bool) -> R<(), Io::Error> {
    with_bus(io, port, device, |io| {
        if lock { sync::acquire(io, port, mask(port))?; }
        let mut result = Err(Error::I2c { device });
        for _ in 0..2 {
            result = attempt(io, port, device, &mut |bus| {
                bus.start()?;
                bus.send(device, device)?;
                bus.send(device, offset)?;
                bus.send(device, value)?;
                bus.stop()
            });
            if !matches!(result, Err(Error::I2c { .. })) { break; }
        }
        if lock { sync::release(io, port, mask(port))?; }
        result
    })
}

/// One's-complement sum, carry folded back in (spec 3.6).
fn ones_add(a: u8, b: u8) -> u8 {
    let sum = a as u16 + b as u16;
    ((sum & 0xff) + (sum >> 8)) as u8
}

/// "Combined" 16-bit register read (spec 3.6): a 15-bit address with a read
/// flag and checksum; data high and low each ACKed, the device's checksum
/// byte (not verified, as in the source) NACKed. 4 attempts, the semaphore
/// taken per attempt when `lock`, no delay between them.
pub fn read_combined<Io: Registers>(io: &mut Io, port: Port, device: u8, reg: u16, lock: bool)
    -> R<u16, Io::Error> {
    let high = ((reg >> 7) as u8 & 0xfe) | 1;
    let csum = !ones_add(high, reg as u8);
    for _ in 0..4 {
        if lock { sync::acquire(io, port, mask(port))?; }
        let result = attempt(io, port, device, &mut |bus| {
            bus.start()?;
            for b in [device, high, reg as u8, csum] { bus.send(device, b)?; }
            bus.start()?;
            bus.send(device, device | 1)?;
            let hi = bus.in_byte()?;
            bus.out_bit(false)?;
            let lo = bus.in_byte()?;
            bus.out_bit(false)?;
            bus.in_byte()?;
            bus.out_bit(true)?;
            bus.stop()?;
            Ok((hi as u16) << 8 | lo as u16)
        });
        if lock { sync::release(io, port, mask(port))?; }
        if !matches!(result, Err(Error::I2c { .. })) { return result; }
    }
    Err(Error::I2c { device })
}

/// "Combined" write (spec 3.6): address with the write flag, data high and
/// low, checksum over all four; 2 attempts, the semaphore held across both.
pub fn write_combined<Io: Registers>(io: &mut Io, port: Port, device: u8, reg: u16, value: u16,
    lock: bool) -> R<(), Io::Error> {
    let high = (reg >> 7) as u8 & 0xfe;
    let csum = !ones_add(ones_add(ones_add(high, reg as u8), (value >> 8) as u8), value as u8);
    if lock { sync::acquire(io, port, mask(port))?; }
    let mut result = Err(Error::I2c { device });
    for _ in 0..2 {
        result = attempt(io, port, device, &mut |bus| {
            bus.start()?;
            for b in [device, high, reg as u8, (value >> 8) as u8, value as u8, csum] {
                bus.send(device, b)?;
            }
            bus.stop()
        });
        if !matches!(result, Err(Error::I2c { .. })) { break; }
    }
    if lock { sync::release(io, port, mask(port))?; }
    result
}
