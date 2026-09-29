//! X552 (Xeon D-1500 integrated 10 GbE) link setup, per device ID.
//!
//! Source: Intel's shared ixgbe code as Intel publishes it under the
//! BSD-3-Clause licence (FreeBSD `sys/dev/ixgbe`: ixgbe_x550.c, ixgbe_x540.c,
//! ixgbe_phy.c/.h, ixgbe_type.h; Copyright (c) 2001-2020 Intel Corporation),
//! read for register addresses, fields and sequences where the public Xeon D
//! datasheet has none (owner, #2). Not a translation of it: see
//! docs/bring-up.md for the notice and what differs (no semaphore stealing,
//! no PHY reset, no advertisement rewrite).
//!
//! The five X552 physical functions differ only in what sits between the MAC
//! and the wire:
//!
//! - 15aa KX4 backplane: the hardware runs the link; nothing to write.
//! - 15ab KR backplane: the integrated KR PHY advertises KR (10G) and KX (1G)
//!   and restarts auto-negotiation, through the IOSF sideband.
//! - 15ac SFP+: an Inphi CS4227 retimer behind I2C (bit-banged through
//!   I2CCTL); check it has been reset once since power-on, identify the SFP+
//!   module, force the KR PHY to the module's speed and set the CS4227 line
//!   side equalisation for it.
//! - 15ad 10GBASE-T: an external X557 PHY on MDIO; release its power-up
//!   stall, then run the internal link as iXFI at the copper speed (or KR, if
//!   the NVM selects KR for the internal link).
//! - 15ae 1000BASE-T: the external 1G PHY is run by firmware; nothing to write.

use super::{read, write, Error, Registers};

const ESDP: u32 = 0x00020;
const HLREG0: u32 = 0x04240;
const MSCA: u32 = 0x0425c;
const MSRWD: u32 = 0x04260;
const LINKS: u32 = 0x042a4;
const MMNGC: u32 = 0x042d0;
const SWSM: u32 = 0x10140;
const SWFW_SYNC: u32 = 0x10160;
const IOSF_CTRL: u32 = 0x11144;
const IOSF_DATA: u32 = 0x11148;
const NW_MNG_IF_SEL: u32 = 0x11178;
const I2CCTL: u32 = 0x15f5c;

// SWSM / SW_FW_SYNC.
const SMBI: u32 = 1 << 0;
const REGSMP: u32 = 1 << 31;
const SM_PHY0: u32 = 1 << 1;
const SM_PHY1: u32 = 1 << 2;
const SM_I2C: u32 = 3 << 11;
/// Both PHYs and both I2C buses: the CS4227 and the SFP+ cages are shared.
const SM_SHARED_I2C: u32 = SM_PHY0 | SM_PHY1 | SM_I2C;

// IOSF sideband control.
const IOSF_BUSY: u32 = 1 << 31;
const IOSF_RESP_STAT: u32 = 3 << 18;
/// Target select 0 (bits 30:28) is the KR PHY.
const IOSF_TARGET_KR_PHY: u32 = 0 << 28;

// KR PHY (KRM) registers, per port.
fn krm(lan: u8, port0: u32) -> u32 { if lan == 1 { port0 + 0x4000 } else { port0 } }
const KRM_LINK_CTRL_1: u32 = 0x420c;
const KRM_DSP_TXFFE_STATE_4: u32 = 0x4634;
const KRM_DSP_TXFFE_STATE_5: u32 = 0x4638;
const KRM_RX_TRN_LINKUP_CTRL: u32 = 0x4b00;
const KRM_TX_COEFF_CTRL_1: u32 = 0x5520;

const LC1_FORCE_SPEED: u32 = 7 << 8;
const LC1_FORCE_1G: u32 = 2 << 8;
const LC1_FORCE_10G: u32 = 4 << 8;
const LC1_CAP_KX: u32 = 1 << 16;
const LC1_CAP_KR: u32 = 1 << 18;
const LC1_AN_ENABLE: u32 = 1 << 29;
const LC1_AN_RESTART: u32 = 1 << 31;
const TXFFE_ADAPT: u32 = (1 << 6) | (1 << 15) | (1 << 16);
const TRN_CONV_WO_PROTOCOL: u32 = 1 << 4;
const TX_COEFF_OVERRIDE: u32 = (1 << 31) | (1 << 3) | (1 << 2) | (1 << 1);

const MNG_VETO: u32 = 1 << 0;
const MDCSPD: u32 = 1 << 16;
const INT_PHY_MODE: u32 = 1 << 24;

// MDIO (clause 45) through MSCA/MSRWD.
const MDI_COMMAND: u32 = 1 << 30;
const MDI_READ: u32 = 3 << 26;
const MDI_WRITE: u32 = 1 << 26;
const DEV_PMA: u8 = 1;
const DEV_AN: u8 = 7;
const DEV_VENDOR_1: u8 = 0x1e;
const PMA_TX_VENDOR_ALARMS_3: u16 = 0xcc02;
const VENDOR_GLOBAL_RES_PR_10: u16 = 0xc479;
const POWER_UP_STALL: u16 = 0x8000;
const AN_STATUS: u16 = 0x0001;
const AN_LINK_STATUS: u16 = 1 << 2;
const AN_VENDOR_STATUS: u16 = 0xc800;

// I2CCTL bit-bang bits (X550 layout).
const I2C_BB_EN: u32 = 1 << 8;
const I2C_CLK_OUT: u32 = 1 << 9;
const I2C_DATA_OUT: u32 = 1 << 10;
const I2C_DATA_OE_N: u32 = 1 << 11;
const I2C_DATA_IN: u32 = 1 << 12;
const I2C_CLK_OE_N: u32 = 1 << 13;
const I2C_CLK_IN: u32 = 1 << 14;

// I2C devices.
const SFP_EEPROM: u8 = 0xa0;
const PORT_EXPANDER: u8 = 0xe0;
const CS4227: u8 = 0xbe;
const PE_OUTPUT: u8 = 1;
const PE_CONFIG: u8 = 3;
const PE_CS4227_RESET: u8 = 1 << 1;
const CS_SCRATCH: u16 = 0x0002;
const CS_EFUSE_STATUS: u16 = 0x0181;
const CS_LINE_SPARE24_LSB: u16 = 0x12b0;
const CS_EEPROM_STATUS: u16 = 0x5001;
const CS_LOAD_OK: u16 = 0x0001;
const CS_RESET_PENDING: u16 = 0x1357;
const CS_RESET_COMPLETE: u16 = 0x5aa5;
const CS_RETRIES: usize = 15;
const CS_EDC_CX1: u16 = 0x0002;
const CS_EDC_SR: u16 = 0x0004;

type R<T, E> = Result<T, Error<E>>;

fn delay_ms<Io: Registers>(io: &mut Io, ms: usize) { io.delay_us(ms * 1000); }

// ---- SW/FW semaphore -------------------------------------------------------

/// Take SWSM.SMBI then SW_FW_SYNC.REGSMP, the register-level lock that
/// guards SW_FW_SYNC itself. Reading either bit as 0 grants it.
fn lock_sync<Io: Registers>(io: &mut Io) -> R<(), Io::Error> {
    let mut smbi = false;
    for _ in 0..2000 {
        if read(io, SWSM)? & SMBI == 0 { smbi = true; break; }
        io.delay_us(50);
    }
    if !smbi { return Err(Error::Semaphore { held: SMBI }); }
    for _ in 0..2000 {
        if read(io, SWFW_SYNC)? & REGSMP == 0 { return Ok(()); }
        io.delay_us(50);
    }
    unlock_sync(io)?;
    Err(Error::Semaphore { held: REGSMP })
}

fn unlock_sync<Io: Registers>(io: &mut Io) -> R<(), Io::Error> {
    let sync = read(io, SWFW_SYNC)?;
    write(io, SWFW_SYNC, sync & !REGSMP)?;
    let swsm = read(io, SWSM)?;
    write(io, SWSM, swsm & !SMBI)
}

/// The software bits `mask` takes, and the firmware bits that block it.
fn sync_bits(mask: u32) -> (u32, u32) {
    let sw = mask & (0xf | SM_I2C);
    let fw = ((mask & 0xf) << 5) | ((mask & SM_I2C) << 2);
    (sw, fw)
}

/// Acquire `mask` in SW_FW_SYNC, waiting up to 5 s for firmware or another
/// driver to release it. Never takes a resource another owner holds, even a
/// stuck one: that fails closed as `Error::Semaphore`.
fn acquire<Io: Registers>(io: &mut Io, lan: u8, mask: u32) -> R<(), Io::Error> {
    let (sw, fw) = sync_bits(mask);
    let mut held = 0;
    for _ in 0..1000 {
        lock_sync(io)?;
        let sync = read(io, SWFW_SYNC)?;
        held = sync & (sw | fw);
        if held == 0 {
            write(io, SWFW_SYNC, sync | sw)?;
            unlock_sync(io)?;
            if mask & SM_I2C != 0 { set_mux(io, lan, true)?; }
            return Ok(());
        }
        unlock_sync(io)?;
        delay_ms(io, 5);
    }
    Err(Error::Semaphore { held })
}

fn release<Io: Registers>(io: &mut Io, lan: u8, mask: u32) -> R<(), Io::Error> {
    if mask & SM_I2C != 0 { set_mux(io, lan, false)?; }
    let (sw, _) = sync_bits(mask);
    lock_sync(io)?;
    let sync = read(io, SWFW_SYNC)?;
    write(io, SWFW_SYNC, sync & !sw)?;
    unlock_sync(io)?;
    io.delay_us(10);
    Ok(())
}

/// Run `f` holding `mask`; release it whether or not `f` succeeded.
fn locked<Io: Registers, T>(io: &mut Io, lan: u8, mask: u32,
    f: impl FnOnce(&mut Io) -> R<T, Io::Error>) -> R<T, Io::Error> {
    acquire(io, lan, mask)?;
    let result = f(io);
    let released = release(io, lan, mask);
    let value = result?;
    released?;
    Ok(value)
}

/// Port 1 reaches the shared I2C bus through a mux on SDP1.
fn set_mux<Io: Registers>(io: &mut Io, lan: u8, on: bool) -> R<(), Io::Error> {
    if lan == 0 { return Ok(()); }
    let esdp = read(io, ESDP)?;
    write(io, ESDP, if on { esdp | (1 << 1) } else { esdp & !(1 << 1) })
}

/// SDPs as I2C mux control: SDP0 input, and on port 1 SDP1 a GPIO output.
fn setup_mux_ctl<Io: Registers>(io: &mut Io, lan: u8) -> R<(), Io::Error> {
    let mut esdp = read(io, ESDP)?;
    if lan == 1 {
        esdp &= !((1 << 17) | (1 << 1));
        esdp |= 1 << 9;
    }
    esdp &= !((1 << 16) | (1 << 8));
    write(io, ESDP, esdp)
}

// ---- IOSF sideband (KR PHY registers) --------------------------------------

fn iosf_idle<Io: Registers>(io: &mut Io) -> R<u32, Io::Error> {
    let mut last = 0;
    for _ in 0..100 {
        last = io.read(IOSF_CTRL).map_err(Error::Io)?;
        if last & IOSF_BUSY == 0 { return Ok(last); }
        io.delay_us(10);
    }
    Err(Error::Timeout { register: IOSF_CTRL, mask: IOSF_BUSY, expected: 0, last })
}

fn iosf_check<E>(address: u32, ctrl: u32) -> R<(), E> {
    if ctrl & IOSF_RESP_STAT != 0 { return Err(Error::Sideband { address, ctrl }); }
    Ok(())
}

fn kr_read<Io: Registers>(io: &mut Io, lan: u8, address: u32) -> R<u32, Io::Error> {
    locked(io, lan, SM_PHY0 | SM_PHY1, |io| {
        iosf_idle(io)?;
        write(io, IOSF_CTRL, address | IOSF_TARGET_KR_PHY)?;
        iosf_check(address, iosf_idle(io)?)?;
        io.read(IOSF_DATA).map_err(Error::Io)
    })
}

fn kr_write<Io: Registers>(io: &mut Io, lan: u8, address: u32, value: u32) -> R<(), Io::Error> {
    locked(io, lan, SM_PHY0 | SM_PHY1, |io| {
        iosf_idle(io)?;
        write(io, IOSF_CTRL, address | IOSF_TARGET_KR_PHY)?;
        write(io, IOSF_DATA, value)?;
        iosf_check(address, iosf_idle(io)?)
    })
}

fn kr_modify<Io: Registers>(io: &mut Io, lan: u8, port0: u32,
    f: impl FnOnce(u32) -> u32) -> R<u32, Io::Error> {
    let address = krm(lan, port0);
    let value = f(kr_read(io, lan, address)?);
    kr_write(io, lan, address, value)?;
    Ok(value)
}

fn restart_an<Io: Registers>(io: &mut Io, lan: u8) -> R<u32, Io::Error> {
    kr_modify(io, lan, KRM_LINK_CTRL_1, |v| v | LC1_AN_RESTART)
}

/// Auto-negotiate the KR PHY, advertising KR (10G) and/or KX (1G).
fn kr_autoneg<Io: Registers>(io: &mut Io, lan: u8, kr: bool, kx: bool) -> R<u32, Io::Error> {
    let lc1 = kr_modify(io, lan, KRM_LINK_CTRL_1, |mut v| {
        v |= LC1_AN_ENABLE;
        v &= !(LC1_CAP_KR | LC1_CAP_KX);
        if kr { v |= LC1_CAP_KR; }
        if kx { v |= LC1_CAP_KX; }
        v
    })?;
    restart_an(io, lan)?;
    Ok(lc1)
}

/// iXFI: the KR PHY forced to one speed without auto-negotiation, with the
/// KR training and TX FFE adaptation off and fixed TX coefficients, then a
/// port reset through AN restart.
fn ixfi<Io: Registers>(io: &mut Io, lan: u8, ten_gig: bool) -> R<u32, Io::Error> {
    let lc1 = kr_modify(io, lan, KRM_LINK_CTRL_1, |mut v| {
        v &= !(LC1_AN_ENABLE | LC1_FORCE_SPEED);
        v | if ten_gig { LC1_FORCE_10G } else { LC1_FORCE_1G }
    })?;
    kr_modify(io, lan, KRM_RX_TRN_LINKUP_CTRL, |v| v | TRN_CONV_WO_PROTOCOL)?;
    kr_modify(io, lan, KRM_DSP_TXFFE_STATE_4, |v| v & !TXFFE_ADAPT)?;
    kr_modify(io, lan, KRM_DSP_TXFFE_STATE_5, |v| v & !TXFFE_ADAPT)?;
    kr_modify(io, lan, KRM_TX_COEFF_CTRL_1, |v| v | TX_COEFF_OVERRIDE)?;
    restart_an(io, lan)?;
    Ok(lc1)
}

// ---- MDIO (external X557) --------------------------------------------------

fn phy_mask(lan: u8) -> u32 { if lan == 1 { SM_PHY1 } else { SM_PHY0 } }

fn mdi<Io: Registers>(io: &mut Io, command: u32) -> R<(), Io::Error> {
    write(io, MSCA, command | MDI_COMMAND)?;
    let mut last = 0;
    for _ in 0..100 {
        io.delay_us(10);
        last = read(io, MSCA)?;
        if last & MDI_COMMAND == 0 { return Ok(()); }
    }
    Err(Error::Timeout { register: MSCA, mask: MDI_COMMAND, expected: 0, last })
}

fn mdio_addr(phy: u8, dev: u8, reg: u16) -> u32 {
    reg as u32 | (dev as u32) << 16 | (phy as u32) << 21
}

fn mdio_read<Io: Registers>(io: &mut Io, lan: u8, phy: u8, dev: u8, reg: u16) -> R<u16, Io::Error> {
    locked(io, lan, phy_mask(lan), |io| {
        let a = mdio_addr(phy, dev, reg);
        mdi(io, a)?;
        mdi(io, a | MDI_READ)?;
        Ok((io.read(MSRWD).map_err(Error::Io)? >> 16) as u16)
    })
}

fn mdio_write<Io: Registers>(io: &mut Io, lan: u8, phy: u8, dev: u8, reg: u16, value: u16) -> R<(), Io::Error> {
    locked(io, lan, phy_mask(lan), |io| {
        write(io, MSRWD, value as u32)?;
        let a = mdio_addr(phy, dev, reg);
        mdi(io, a)?;
        mdi(io, a | MDI_WRITE)
    })
}

/// A PHY answers at `phy` if its PMA identifier is neither 0 nor all ones.
fn probe_phy<Io: Registers>(io: &mut Io, lan: u8, phy: u8) -> R<Option<u32>, Io::Error> {
    let high = mdio_read(io, lan, phy, DEV_PMA, 2)?;
    if high == 0 || high == 0xffff { return Ok(None); }
    let low = mdio_read(io, lan, phy, DEV_PMA, 3)?;
    Ok(Some((high as u32) << 16 | low as u32))
}

/// Copper link as the X557 sees it. AN status link is latched low: the
/// second of two back-to-back reads is the current state.
fn copper_up<Io: Registers>(io: &mut Io, lan: u8, phy: u8) -> R<bool, Io::Error> {
    mdio_read(io, lan, phy, DEV_AN, AN_STATUS)?;
    Ok(mdio_read(io, lan, phy, DEV_AN, AN_STATUS)? & AN_LINK_STATUS != 0)
}

// ---- I2C bit-bang through I2CCTL -------------------------------------------
//
// Standard-mode timing (4.7 us low, 4 us high). SCL is released (CLK_OE_N)
// and driven high, waiting up to 500 us for a stretching slave; SDA is driven
// low or released (DATA_OE_N) and read back through DATA_IN.

struct I2c<'a, Io> { io: &'a mut Io, ctl: u32 }

impl<'a, Io: Registers> I2c<'a, Io> {
    fn new(io: &'a mut Io) -> R<Self, Io::Error> {
        let ctl = io.read(I2CCTL).map_err(Error::Io)?;
        Ok(I2c { io, ctl })
    }
    fn put(&mut self) -> R<(), Io::Error> { self.io.write(I2CCTL, self.ctl).map_err(Error::Io) }
    fn get(&mut self) -> R<u32, Io::Error> { self.io.read(I2CCTL).map_err(Error::Io) }

    fn scl_high(&mut self) -> R<(), Io::Error> {
        self.ctl |= I2C_CLK_OE_N;
        self.put()?;
        for _ in 0..500 {
            self.ctl |= I2C_CLK_OUT;
            self.put()?;
            self.io.delay_us(1);
            if self.get()? & I2C_CLK_IN != 0 { break; }
        }
        Ok(())
    }
    fn scl_low(&mut self) -> R<(), Io::Error> {
        self.ctl &= !(I2C_CLK_OUT | I2C_CLK_OE_N);
        self.put()?;
        self.io.delay_us(1);
        Ok(())
    }
    /// Drive SDA low, or release it high and check nothing holds it low.
    fn sda(&mut self, high: bool) -> R<(), Io::Error> {
        if high { self.ctl |= I2C_DATA_OUT } else { self.ctl &= !I2C_DATA_OUT }
        self.ctl &= !I2C_DATA_OE_N;
        self.put()?;
        self.io.delay_us(3);
        if !high { return Ok(()); }
        self.ctl |= I2C_DATA_OE_N;
        self.put()?;
        if self.get()? & I2C_DATA_IN == 0 { return Err(Error::I2c { device: 0 }); }
        Ok(())
    }
    fn release_sda(&mut self) -> R<(), Io::Error> {
        self.ctl |= I2C_DATA_OUT | I2C_DATA_OE_N;
        self.put()
    }
    fn start(&mut self) -> R<(), Io::Error> {
        self.ctl = self.get()? | I2C_BB_EN;
        self.sda(true)?;
        self.scl_high()?;
        self.io.delay_us(5);
        self.sda(false)?;
        self.io.delay_us(4);
        self.scl_low()?;
        self.io.delay_us(5);
        Ok(())
    }
    fn stop(&mut self) -> R<(), Io::Error> {
        self.ctl = self.get()?;
        self.sda(false)?;
        self.scl_high()?;
        self.io.delay_us(4);
        self.sda(true)?;
        self.io.delay_us(5);
        self.ctl &= !I2C_BB_EN;
        self.ctl |= I2C_DATA_OE_N | I2C_CLK_OE_N;
        self.put()
    }
    fn clock(&mut self) -> R<bool, Io::Error> {
        self.scl_high()?;
        self.io.delay_us(4);
        let sda = self.get()? & I2C_DATA_IN != 0;
        self.scl_low()?;
        self.io.delay_us(5);
        Ok(sda)
    }
    fn out_bit(&mut self, bit: bool) -> R<(), Io::Error> {
        self.sda(bit)?;
        self.clock().map(|_| ())
    }
    fn in_byte(&mut self) -> R<u8, Io::Error> {
        self.release_sda()?;
        let mut byte = 0;
        for _ in 0..8 { byte = byte << 1 | self.clock()? as u8; }
        Ok(byte)
    }
    /// Send a byte and require the slave's ACK (SDA low on the ninth clock).
    fn out_byte(&mut self, device: u8, byte: u8) -> R<(), Io::Error> {
        for i in (0..8).rev() { self.out_bit(byte >> i & 1 != 0)?; }
        self.release_sda()?;
        if self.clock()? { return Err(Error::I2c { device }); }
        Ok(())
    }
    /// Nine clocks with SDA released free a slave stuck mid-byte.
    fn clear(&mut self) -> R<(), Io::Error> {
        self.start()?;
        self.release_sda()?;
        for _ in 0..9 { self.clock()?; }
        self.start()?;
        self.stop()
    }
}

/// Run one I2C transaction, clearing the bus and retrying on failure.
fn i2c_try<Io: Registers, T>(io: &mut Io, device: u8, attempts: usize,
    mut f: impl FnMut(&mut I2c<'_, Io>) -> R<T, Io::Error>) -> R<T, Io::Error> {
    let mut last = Error::I2c { device };
    for _ in 0..attempts {
        let mut bus = I2c::new(io)?;
        match f(&mut bus) {
            Ok(v) => return Ok(v),
            Err(Error::I2c { .. }) => {
                last = Error::I2c { device };
                match bus.clear() {
                    Ok(()) | Err(Error::I2c { .. }) => {}
                    Err(e) => return Err(e),
                }
            }
            Err(e) => return Err(e),
        }
    }
    Err(last)
}

/// Byte read: address + register, repeated start, one byte, NACK.
fn i2c_read_byte<Io: Registers>(io: &mut Io, device: u8, reg: u8, attempts: usize) -> R<u8, Io::Error> {
    i2c_try(io, device, attempts, |bus| {
        bus.start()?;
        bus.out_byte(device, device)?;
        bus.out_byte(device, reg)?;
        bus.start()?;
        bus.out_byte(device, device | 1)?;
        let value = bus.in_byte()?;
        bus.out_bit(true)?;
        bus.stop()?;
        Ok(value)
    })
}

fn i2c_write_byte<Io: Registers>(io: &mut Io, device: u8, reg: u8, value: u8) -> R<(), Io::Error> {
    i2c_try(io, device, 2, |bus| {
        bus.start()?;
        bus.out_byte(device, device)?;
        bus.out_byte(device, reg)?;
        bus.out_byte(device, value)?;
        bus.stop()
    })
}

fn ones_add(a: u8, b: u8) -> u8 {
    let sum = a as u16 + b as u16;
    ((sum & 0xff) + (sum >> 8)) as u8
}

/// CS4227 "combined" 16-bit register read: 15-bit register address with a
/// read flag, one's-complement checksum, then data high, low and checksum.
fn cs_read<Io: Registers>(io: &mut Io, reg: u16) -> R<u16, Io::Error> {
    let high = ((reg >> 7) as u8 & 0xfe) | 1;
    let csum = !ones_add(high, reg as u8);
    i2c_try(io, CS4227, 4, |bus| {
        bus.start()?;
        for b in [CS4227, high, reg as u8, csum] { bus.out_byte(CS4227, b)?; }
        bus.start()?;
        bus.out_byte(CS4227, CS4227 | 1)?;
        let hi = bus.in_byte()?;
        bus.out_bit(false)?;
        let lo = bus.in_byte()?;
        bus.out_bit(false)?;
        bus.in_byte()?;
        bus.out_bit(false)?;
        bus.stop()?;
        Ok((hi as u16) << 8 | lo as u16)
    })
}

fn cs_write<Io: Registers>(io: &mut Io, reg: u16, value: u16) -> R<(), Io::Error> {
    let high = (reg >> 7) as u8 & 0xfe;
    let mut csum = ones_add(high, reg as u8);
    csum = ones_add(csum, (value >> 8) as u8);
    csum = !ones_add(csum, value as u8);
    i2c_try(io, CS4227, 2, |bus| {
        bus.start()?;
        for b in [CS4227, high, reg as u8, (value >> 8) as u8, value as u8, csum] {
            bus.out_byte(CS4227, b)?;
        }
        bus.stop()
    })
}

// ---- CS4227 and the SFP+ module --------------------------------------------

/// Hard-reset the CS4227 through the port expander's bit 1 (output, low for
/// 500 us), then wait for its EEPROM image to load. Caller holds the lock.
fn reset_cs4227<Io: Registers>(io: &mut Io) -> R<(), Io::Error> {
    let out = i2c_read_byte(io, PORT_EXPANDER, PE_OUTPUT, 4)?;
    i2c_write_byte(io, PORT_EXPANDER, PE_OUTPUT, out | PE_CS4227_RESET)?;
    let cfg = i2c_read_byte(io, PORT_EXPANDER, PE_CONFIG, 4)?;
    i2c_write_byte(io, PORT_EXPANDER, PE_CONFIG, cfg & !PE_CS4227_RESET)?;
    let out = i2c_read_byte(io, PORT_EXPANDER, PE_OUTPUT, 4)?;
    i2c_write_byte(io, PORT_EXPANDER, PE_OUTPUT, out & !PE_CS4227_RESET)?;
    io.delay_us(500);
    let out = i2c_read_byte(io, PORT_EXPANDER, PE_OUTPUT, 4)?;
    i2c_write_byte(io, PORT_EXPANDER, PE_OUTPUT, out | PE_CS4227_RESET)?;
    delay_ms(io, 450);
    let mut efuse = 0;
    for _ in 0..CS_RETRIES {
        if let Ok(v) = cs_read(io, CS_EFUSE_STATUS) {
            efuse = v;
            if v == CS_LOAD_OK { break; }
        }
        delay_ms(io, 30);
    }
    if efuse != CS_LOAD_OK { return Err(Error::Cs4227 { register: CS_EFUSE_STATUS, value: efuse }); }
    let eeprom = cs_read(io, CS_EEPROM_STATUS)?;
    if eeprom & CS_LOAD_OK == 0 { return Err(Error::Cs4227 { register: CS_EEPROM_STATUS, value: eeprom }); }
    Ok(())
}

/// The CS4227 is shared by both ports: the first driver instance since
/// power-on resets it and records that in its scratch register; the other
/// waits for the record. Returns whether this call did the reset.
fn check_cs4227<Io: Registers>(io: &mut Io, lan: u8) -> R<bool, Io::Error> {
    let mut held = false;
    for _ in 0..CS_RETRIES {
        acquire(io, lan, SM_SHARED_I2C)?;
        let scratch = cs_read(io, CS_SCRATCH);
        match scratch {
            Ok(CS_RESET_COMPLETE) => { release(io, lan, SM_SHARED_I2C)?; return Ok(false); }
            Ok(CS_RESET_PENDING) => {
                release(io, lan, SM_SHARED_I2C)?;
                delay_ms(io, 30);
            }
            Ok(_) | Err(Error::I2c { .. }) => { held = true; break; }
            Err(e) => { release(io, lan, SM_SHARED_I2C)?; return Err(e); }
        }
    }
    // Still pending after all retries: the other instance is taken to have failed.
    if !held { acquire(io, lan, SM_SHARED_I2C)?; }
    if let Err(e) = reset_cs4227(io) {
        release(io, lan, SM_SHARED_I2C)?;
        return Err(e);
    }
    // The reset is long; let a waiting instance see it pending meanwhile.
    let pending = cs_write(io, CS_SCRATCH, CS_RESET_PENDING);
    release(io, lan, SM_SHARED_I2C)?;
    pending?;
    delay_ms(io, 10);
    locked(io, lan, SM_SHARED_I2C, |io| cs_write(io, CS_SCRATCH, CS_RESET_COMPLETE))?;
    Ok(true)
}

/// SFP+ module, from its SFF-8472 ID EEPROM (identifier, 10G and 1G
/// compliance codes, cable technology).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Module {
    /// Nothing answered at the module EEPROM address.
    Absent,
    /// Passive direct-attach copper: linear equalisation, 10G.
    DirectAttachPassive,
    /// Active limiting direct-attach, 10G.
    DirectAttachActive,
    /// 10GBASE-SR/LR (or 10G BX) optics.
    Optical10g,
    /// 1000BASE-SX/LX/BX optics.
    Optical1g,
    /// Not an SFP, 1000BASE-T, or unknown compliance: not driven.
    Unsupported { identifier: u8, comp_10g: u8, comp_1g: u8, cable: u8 },
}

impl Module {
    fn ten_gig(self) -> bool { !matches!(self, Module::Optical1g) }
    fn linear(self) -> bool { matches!(self, Module::DirectAttachPassive) }
}

fn sfp_byte<Io: Registers>(io: &mut Io, lan: u8, offset: u8, attempts: usize) -> R<u8, Io::Error> {
    locked(io, lan, SM_SHARED_I2C, |io| i2c_read_byte(io, SFP_EEPROM, offset, attempts))
}

fn identify_sfp<Io: Registers>(io: &mut Io, lan: u8) -> R<Module, Io::Error> {
    // A module may take a while to answer after insertion; an empty cage
    // never does.
    let identifier = match sfp_byte(io, lan, 0, 10) {
        Ok(v) => v,
        Err(Error::I2c { .. }) => return Ok(Module::Absent),
        Err(e) => return Err(e),
    };
    let comp_1g = sfp_byte(io, lan, 6, 4)?;
    let comp_10g = sfp_byte(io, lan, 3, 4)?;
    let cable = sfp_byte(io, lan, 8, 4)?;
    let unsupported = Module::Unsupported { identifier, comp_10g, comp_1g, cable };
    if identifier != 0x03 { return Ok(unsupported); }
    Ok(if cable & 0x04 != 0 {
        Module::DirectAttachPassive
    } else if cable & 0x08 != 0 {
        // Active DA is driven only as a limiting (SFF-8431 appendix E) cable.
        if sfp_byte(io, lan, 0x3c, 4)? & 0x04 != 0 { Module::DirectAttachActive } else { unsupported }
    } else if comp_10g & 0x30 != 0 {
        Module::Optical10g
    } else if comp_1g & 0x08 != 0 {
        unsupported // 1000BASE-T SFP: not supported behind the CS4227.
    } else if comp_1g & (0x01 | 0x02 | 0x40) != 0 {
        Module::Optical1g
    } else if comp_10g == 0 && sfp_byte(io, lan, 12, 4)? == 0x67
        && (sfp_byte(io, lan, 14, 4)? > 0 || sfp_byte(io, lan, 15, 4)? >= 10) {
        Module::Optical10g // 10G BX: 10.3 GBd, single-mode reach, no 10G code.
    } else {
        unsupported
    })
}

// ---- Per-device setup ------------------------------------------------------

/// Which internal (MAC-to-X557) link mode the NVM selects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Internal {
    /// iXFI, forced to the copper speed (NW_MNG_IF_SEL.INT_PHY_MODE = 0).
    Ixfi,
    /// KR auto-negotiation (INT_PHY_MODE = 1).
    Kr,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Setup {
    /// 15aa: the KX4 link is run by the hardware; nothing written.
    Kx4,
    /// 15ae: the external 1G PHY is run by firmware; nothing written.
    FirmwarePhy,
    /// 15ab: KR PHY auto-negotiating KR and KX (KRM LINK_CTRL_1 as written).
    Kr { link_ctrl: u32 },
    /// 15ab: manageability firmware has vetoed link changes (MMNGC.MNG_VETO).
    ManageabilityVeto,
    /// 15ac: `module` found. For a supported module the KR PHY was set to
    /// its speed and the CS4227 line side to `edc` (CX1 or SR).
    Sfp { module: Module, cs4227_reset: bool, link_ctrl: Option<u32>, edc: Option<u16> },
    /// 15ad: X557 at MDIO address `phy` with identifier `id`; `unstalled`
    /// when this was the first start since power-on.
    Copper { phy: u8, id: u32, unstalled: bool, internal: Internal },
}

/// X552 link setup after `reset`, by device ID.
pub fn setup<Io: Registers>(io: &mut Io, device: u16, lan: u8) -> R<Setup, Io::Error> {
    match device {
        0x15ab => {
            if read(io, MMNGC)? & MNG_VETO != 0 { return Ok(Setup::ManageabilityVeto); }
            Ok(Setup::Kr { link_ctrl: kr_autoneg(io, lan, true, true)? })
        }
        0x15ac => setup_sfp(io, lan),
        0x15ad => setup_copper(io, lan),
        0x15ae => Ok(Setup::FirmwarePhy),
        _ => Ok(Setup::Kx4),
    }
}

fn setup_sfp<Io: Registers>(io: &mut Io, lan: u8) -> R<Setup, Io::Error> {
    setup_mux_ctl(io, lan)?;
    let cs4227_reset = check_cs4227(io, lan)?;
    let module = identify_sfp(io, lan)?;
    if matches!(module, Module::Absent | Module::Unsupported { .. }) {
        return Ok(Setup::Sfp { module, cs4227_reset, link_ctrl: None, edc: None });
    }
    // The CS4227 does not auto-negotiate: advertise only the module's speed.
    let ten = module.ten_gig();
    let link_ctrl = kr_autoneg(io, lan, ten, !ten)?;
    let edc = if module.linear() { CS_EDC_CX1 } else { CS_EDC_SR };
    let value = (edc << 1) | 1;
    locked(io, lan, SM_SHARED_I2C, |io| cs_write(io, CS_LINE_SPARE24_LSB + ((lan as u16) << 12), value))?;
    Ok(Setup::Sfp { module, cs4227_reset, link_ctrl: Some(link_ctrl), edc: Some(edc) })
}

fn setup_copper<Io: Registers>(io: &mut Io, lan: u8) -> R<Setup, Io::Error> {
    // MDIO clock to the slow (default) rate before the first PHY access.
    let hlreg0 = read(io, HLREG0)?;
    write(io, HLREG0, hlreg0 & !MDCSPD)?;
    let sel = read(io, NW_MNG_IF_SEL)?;
    let (phy, id) = if sel != 0 {
        let phy = ((sel >> 3) & 0x1f) as u8;
        match probe_phy(io, lan, phy)? {
            Some(id) => (phy, id),
            None => return Err(Error::NoPhy),
        }
    } else {
        let mut found = None;
        for phy in 0..32 {
            if let Some(id) = probe_phy(io, lan, phy)? { found = Some((phy, id)); break; }
        }
        found.ok_or(Error::NoPhy)?
    };
    // First start since power-on: the PHY firmware waits, stalled, for the
    // driver to release it.
    let alarms = mdio_read(io, lan, phy, DEV_PMA, PMA_TX_VENDOR_ALARMS_3)?;
    let unstalled = alarms & 3 != 0;
    if unstalled {
        let prov = mdio_read(io, lan, phy, DEV_VENDOR_1, VENDOR_GLOBAL_RES_PR_10)?;
        mdio_write(io, lan, phy, DEV_VENDOR_1, VENDOR_GLOBAL_RES_PR_10, prov & !POWER_UP_STALL)?;
    }
    let internal = if sel & INT_PHY_MODE != 0 {
        kr_autoneg(io, lan, true, true)?;
        Internal::Kr
    } else {
        ixfi(io, lan, true)?;
        // Let the MAC-to-PHY link come up (up to 1 s); down is not an error.
        for _ in 0..10 {
            delay_ms(io, 100);
            if read(io, LINKS)? & (1 << 30) != 0 { break; }
        }
        Internal::Ixfi
    };
    Ok(Setup::Copper { phy, id, unstalled, internal })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Copper {
    Down,
    /// Copper up at `megabits`; `retrained` when iXFI was re-forced to 1G.
    Up { megabits: u32, retrained: bool },
    /// Copper up at a speed the internal link cannot carry (10/100 Mb/s).
    Unsupported { status: u16 },
}

/// 15ad: wait up to `millis` for the X557's copper link, then match the
/// internal iXFI link to the copper speed (the X557 does not rate-adapt).
pub fn follow_copper<Io: Registers>(io: &mut Io, lan: u8, phy: u8, internal: Internal,
    millis: usize) -> R<Copper, Io::Error> {
    let mut elapsed = 0;
    while !copper_up(io, lan, phy)? {
        if elapsed >= millis { return Ok(Copper::Down); }
        delay_ms(io, 100);
        elapsed += 100;
    }
    let status = mdio_read(io, lan, phy, DEV_AN, AN_VENDOR_STATUS)?;
    let megabits = match status & 7 {
        7 => 10_000,
        5 => 1000,
        _ => return Ok(Copper::Unsupported { status }),
    };
    let retrained = internal == Internal::Ixfi && megabits == 1000;
    if retrained { ixfi(io, lan, false)?; }
    Ok(Copper::Up { megabits, retrained })
}
