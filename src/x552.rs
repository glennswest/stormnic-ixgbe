//! X552 (Xeon D-1500 integrated 10 GbE) link setup, per device ID (spec 7,
//! walkthroughs 9.6–9.9).
//!
//! The six X552 physical functions differ only in what sits between the MAC
//! and the wire:
//!
//! - 15aa KX4 and 15b0 XFI backplane: the hardware runs the link; nothing to write.
//! - 15ab KR backplane: the integrated KR PHY advertises KR (10G) and KX (1G)
//!   and restarts auto-negotiation through the IOSF sideband (not on veto).
//! - 15ac SFP+: an Inphi CS4227 retimer on a shared I2C segment; reset it
//!   once per power-on, identify the SFP+ module, then per speed set the KR
//!   PHY and the CS4227 line-side EDC mode (10G then 1G for multispeed
//!   modules, with soft rate select).
//! - 15ad 10GBASE-T: an external X557 PHY on MDIO; release its power-up
//!   stall and reset it, run the internal link as forced iXFI (or KR, by
//!   NW_MNG_IF_SEL), advertise 10G + 1G, and re-force the internal link to
//!   the copper speed whenever copper comes up.
//! - 15ae 1000BASE-T: the external Marvell PHY is run by firmware; nothing to write.
//!
//! The X552 NVM is read through the firmware's host interface (`nvm_word`,
//! spec 11.2), not EERD; the SFP device reads the crosstalk-fix word with it.
//!
//! The X553 (`x553`) shares the KR PHY access, the SFP multispeed loop and
//! the X557 path; where its KR PHY differs (spec 12.7), the functions here
//! check the family.

use super::mdio::{self, Speeds};
use super::sfp::{self, Module};
use super::{delay_ms, flush, i2c, read, sync, write, Error, Family, Link, Port, Registers,
    ESDP, HLREG0, LINKS, MMNGC, R};

const HICR: u32 = 0x15f00;
const FWSTS: u32 = 0x15f0c;
const FLEX_MNG: u32 = 0x15800;
const IOSF_CTRL: u32 = 0x11144;
const IOSF_DATA: u32 = 0x11148;
const NW_MNG_IF_SEL: u32 = 0x11178;

// IOSF sideband control (spec 7.2).
const IOSF_BUSY: u32 = 1 << 31;
const IOSF_RESP_STAT: u32 = 3 << 18;
/// Target select 0 (bits 30:28) is the KR PHY.
const IOSF_TARGET_KR_PHY: u32 = 0 << 28;

// KR PHY (KRM) registers: port 0 at 0x4xxx, port 1 at 0x8xxx (spec 7.3).
pub(super) fn krm(lan: u8, port0: u32) -> u32 { if lan == 1 { port0 + 0x4000 } else { port0 } }
pub(super) const KRM_LINK_CTRL_1: u32 = 0x420c;
const KRM_DSP_TXFFE_STATE_4: u32 = 0x4634;
const KRM_DSP_TXFFE_STATE_5: u32 = 0x4638;
const KRM_RX_TRN_LINKUP_CTRL: u32 = 0x4b00;
const KRM_TX_COEFF_CTRL_1: u32 = 0x5520;

pub(super) const LC1_FORCE_SPEED: u32 = 7 << 8;
pub(super) const LC1_FORCE_1G: u32 = 2 << 8;
const LC1_FORCE_10G: u32 = 4 << 8;
pub(super) const LC1_SGMII: u32 = 1 << 12;
pub(super) const LC1_CLAUSE_37: u32 = 1 << 13;
const LC1_CAP_KX: u32 = 1 << 16;
pub(super) const LC1_CAP_KR: u32 = 1 << 18;
pub(super) const LC1_AN_ENABLE: u32 = 1 << 29;
const LC1_AN_RESTART: u32 = 1 << 31;

// X553 only: PMD_FLX_MASK_ST20 (spec 12.7).
pub(super) const KRM_FLX: u32 = 0x5054;
pub(super) const FLX_SFI_SR: u32 = 1 << 20;
pub(super) const FLX_SFI_MODE: u32 = 3 << 20;
pub(super) const FLX_SGMII: u32 = 1 << 25;
pub(super) const FLX_AN37: u32 = 1 << 26;
pub(super) const FLX_AN: u32 = 1 << 27;
pub(super) const FLX_SPEED: u32 = 7 << 28;
pub(super) const FLX_SPEED_1G: u32 = 2 << 28;
pub(super) const FLX_SPEED_10G: u32 = 3 << 28;
pub(super) const FLX_SPEED_AN: u32 = 4 << 28;
const FLX_FW_AN_RESTART: u32 = 1 << 31;
const TXFFE_ADAPT: u32 = (1 << 6) | (1 << 15) | (1 << 16);
const TRN_CONV_WO_PROTOCOL: u32 = 1 << 4;
const TX_COEFF_OVERRIDE: u32 = (1 << 31) | (1 << 3) | (1 << 2) | (1 << 1);

const MNG_VETO: u32 = 1 << 0;
const MDCSPD: u32 = 1 << 16;
const INT_PHY_MODE: u32 = 1 << 24;
const LINK_UP: u32 = 1 << 30;

// X557 (spec 2.7, 7.8).
const PMA_TX_VENDOR_ALARMS_3: u16 = 0xcc02;
const VENDOR_GLOBAL_RES_PR_10: u16 = 0xc479;
const POWER_UP_STALL: u16 = 0x8000;
const AN_VENDOR_STATUS: u16 = 0xc800;

// I2C devices and CS4227 registers (spec 7.7.2).
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

// ---- IOSF sideband (KR PHY registers), spec 7.2 ---------------------------

const KR_LOCK: u32 = sync::PHY0 | sync::PHY1;

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

/// Read: the CTRL write starts it. PHY0 and PHY1 are taken whichever port.
fn kr_read<Io: Registers>(io: &mut Io, port: Port, address: u32) -> R<u32, Io::Error> {
    sync::locked(io, port, KR_LOCK, |io| {
        iosf_idle(io)?;
        write(io, IOSF_CTRL, address | IOSF_TARGET_KR_PHY)?;
        iosf_check(address, iosf_idle(io)?)?;
        io.read(IOSF_DATA).map_err(Error::Io)
    })
}

/// Write: CTRL, then DATA, which starts it (spec 10 item 6: keep this order).
pub(super) fn kr_write<Io: Registers>(io: &mut Io, port: Port, address: u32, value: u32) -> R<(), Io::Error> {
    sync::locked(io, port, KR_LOCK, |io| {
        iosf_idle(io)?;
        write(io, IOSF_CTRL, address | IOSF_TARGET_KR_PHY)?;
        write(io, IOSF_DATA, value)?;
        iosf_check(address, iosf_idle(io)?)
    })
}

pub(super) fn kr_modify<Io: Registers>(io: &mut Io, port: Port, port0: u32,
    f: impl FnOnce(u32) -> u32) -> R<u32, Io::Error> {
    let address = krm(port.lan, port0);
    let value = f(kr_read(io, port, address)?);
    kr_write(io, port, address, value)?;
    Ok(value)
}

/// LINK_CTRL_1.AN_RESTART; also resets the port after forcing a speed. On
/// the X553, then PMD_FLX_MASK_ST20.FW_AN_RESTART, which tells the firmware
/// (spec 12.7).
pub(super) fn restart_an<Io: Registers>(io: &mut Io, port: Port) -> R<u32, Io::Error> {
    let lc1 = kr_modify(io, port, KRM_LINK_CTRL_1, |v| v | LC1_AN_RESTART)?;
    if port.family == Family::X553 {
        kr_modify(io, port, KRM_FLX, |v| v | FLX_FW_AN_RESTART)?;
    }
    Ok(lc1)
}

/// KR auto-negotiation advertising KR (10G) and/or KX (1G), then restart
/// (spec 7.4 steps 3–4). On the X553 the lane is also set to KR AN in
/// PMD_FLX_MASK_ST20 before the restart (spec 12.7). The caller checks the
/// veto where it applies.
pub(super) fn kr_autoneg<Io: Registers>(io: &mut Io, port: Port, kr: bool, kx: bool) -> R<u32, Io::Error> {
    let lc1 = kr_modify(io, port, KRM_LINK_CTRL_1, |mut v| {
        v |= LC1_AN_ENABLE;
        v &= !(LC1_CAP_KR | LC1_CAP_KX);
        if kr { v |= LC1_CAP_KR; }
        if kx { v |= LC1_CAP_KX; }
        v
    })?;
    if port.family == Family::X553 {
        kr_modify(io, port, KRM_FLX, |v| (v & !(FLX_SPEED | FLX_AN37 | FLX_SGMII)) | FLX_SPEED_AN | FLX_AN)?;
    }
    restart_an(io, port)?;
    Ok(lc1)
}

/// iXFI (spec 7.5): the KR PHY forced to one speed without AN, training and
/// TX FFE adaptation off, fixed TX coefficients, then the port reset through
/// AN restart.
fn ixfi<Io: Registers>(io: &mut Io, port: Port, ten_gig: bool) -> R<u32, Io::Error> {
    let lc1 = kr_modify(io, port, KRM_LINK_CTRL_1, |mut v| {
        v &= !(LC1_AN_ENABLE | LC1_FORCE_SPEED);
        v | if ten_gig { LC1_FORCE_10G } else { LC1_FORCE_1G }
    })?;
    kr_modify(io, port, KRM_RX_TRN_LINKUP_CTRL, |v| v | TRN_CONV_WO_PROTOCOL)?;
    kr_modify(io, port, KRM_DSP_TXFFE_STATE_4, |v| v & !TXFFE_ADAPT)?;
    kr_modify(io, port, KRM_DSP_TXFFE_STATE_5, |v| v & !TXFFE_ADAPT)?;
    kr_modify(io, port, KRM_TX_COEFF_CTRL_1, |v| v | TX_COEFF_OVERRIDE)?;
    restart_an(io, port)?;
    Ok(lc1)
}

// ---- SDPs: I2C mux and cage presence, spec 7.7.1 ---------------------------

/// SDP0 an input (cage full) on both ports; on port 1, SDP1 a GPIO output
/// driven low (the mux, which `sync` switches with the I2C bits; the X553
/// runs this too but has no mux, spec 12.11).
pub(super) fn setup_mux_ctl<Io: Registers>(io: &mut Io, lan: u8) -> R<(), Io::Error> {
    let mut esdp = read(io, ESDP)?;
    if lan == 1 {
        esdp &= !((1 << 17) | (1 << 1));
        esdp |= 1 << 9;
    }
    esdp &= !((1 << 16) | (1 << 8));
    write(io, ESDP, esdp)?;
    flush(io)
}

// ---- CS4227, spec 7.7.2 ------------------------------------------------------

fn pe_read<Io: Registers>(io: &mut Io, port: Port, reg: u8) -> R<u8, Io::Error> {
    i2c::read_byte(io, port, PORT_EXPANDER, reg, i2c::attempts(port), false)
}

fn pe_write<Io: Registers>(io: &mut Io, port: Port, reg: u8, value: u8) -> R<(), Io::Error> {
    i2c::write_byte(io, port, PORT_EXPANDER, reg, value, false)
}

/// Hard reset through the port expander's bit 1 (output, low for 500 µs),
/// 450 ms, then the EFUSE and EEPROM load checks. The caller holds the lock.
fn reset_cs4227<Io: Registers>(io: &mut Io, port: Port) -> R<(), Io::Error> {
    let out = pe_read(io, port, PE_OUTPUT)?;
    pe_write(io, port, PE_OUTPUT, out | PE_CS4227_RESET)?;
    let cfg = pe_read(io, port, PE_CONFIG)?;
    pe_write(io, port, PE_CONFIG, cfg & !PE_CS4227_RESET)?;
    let out = pe_read(io, port, PE_OUTPUT)?;
    pe_write(io, port, PE_OUTPUT, out & !PE_CS4227_RESET)?;
    io.delay_us(500);
    let out = pe_read(io, port, PE_OUTPUT)?;
    pe_write(io, port, PE_OUTPUT, out | PE_CS4227_RESET)?;
    delay_ms(io, 450);
    let mut efuse = 0;
    for _ in 0..CS_RETRIES {
        match i2c::read_combined(io, port, CS4227, CS_EFUSE_STATUS, false) {
            Ok(v) => { efuse = v; if v == CS_LOAD_OK { break; } }
            Err(Error::I2c { .. }) => {}
            Err(e) => return Err(e),
        }
        delay_ms(io, 30);
    }
    if efuse != CS_LOAD_OK { return Err(Error::Cs4227 { register: CS_EFUSE_STATUS, value: efuse }); }
    let eeprom = i2c::read_combined(io, port, CS4227, CS_EEPROM_STATUS, false)?;
    if eeprom & CS_LOAD_OK == 0 { return Err(Error::Cs4227 { register: CS_EEPROM_STATUS, value: eeprom }); }
    Ok(())
}

/// Release the shared segment and give firmware and the other port 10 ms.
fn let_go<Io: Registers>(io: &mut Io, port: Port) -> R<(), Io::Error> {
    sync::release(io, port, sync::SHARED_I2C)?;
    delay_ms(io, 10);
    Ok(())
}

/// Check-and-reset (spec 7.7.2): the first port since power-on resets the
/// shared CS4227 and records it in SCRATCH; the other waits for the record.
/// Returns whether this call did the reset.
fn check_cs4227<Io: Registers>(io: &mut Io, port: Port) -> R<bool, Io::Error> {
    let mut held = false;
    for _ in 0..CS_RETRIES {
        match sync::acquire(io, port, sync::SHARED_I2C) {
            Ok(()) => {}
            Err(Error::Semaphore { .. }) => { delay_ms(io, 30); continue; }
            Err(e) => return Err(e),
        }
        match i2c::read_combined(io, port, CS4227, CS_SCRATCH, false) {
            Ok(CS_RESET_COMPLETE) => { let_go(io, port)?; return Ok(false); }
            Ok(CS_RESET_PENDING) => {
                sync::release(io, port, sync::SHARED_I2C)?;
                delay_ms(io, 30);
            }
            Ok(_) | Err(Error::I2c { .. }) => { held = true; break; }
            Err(e) => { sync::release(io, port, sync::SHARED_I2C)?; return Err(e); }
        }
    }
    // Still pending after every try: the other port is taken to have died.
    if !held { sync::acquire(io, port, sync::SHARED_I2C)?; }
    if let Err(e) = reset_cs4227(io, port) {
        let_go(io, port)?;
        return Err(e);
    }
    let pending = i2c::write_combined(io, port, CS4227, CS_SCRATCH, CS_RESET_PENDING, false);
    let_go(io, port)?;
    pending?;
    sync::acquire(io, port, sync::SHARED_I2C)?;
    let complete = i2c::write_combined(io, port, CS4227, CS_SCRATCH, CS_RESET_COMPLETE, false);
    let_go(io, port)?;
    complete?;
    Ok(true)
}

// Host interface (spec 11.2).
const HICR_EN: u32 = 1 << 0;
const HICR_C: u32 = 1 << 1;
const HICR_SV: u32 = 1 << 2;
const FWSTS_FWRI: u32 = 1 << 9;
/// Shadow RAM read: command 0x31, buffer length 6, checksum 0xFF, as the
/// little-endian first dword of the command block.
const READ_SHADOW_RAM: u32 = 0xff06_0031;
const HI_TIMEOUT_US: usize = 500_000;

// ---- NVM -------------------------------------------------------------------

/// One NVM word through the host interface (spec 11.2): the shadow-RAM read
/// command in FLEX_MNG, HICR.C set, C polled clear for up to 500 ms, SV
/// required; the word is the low half of FLEX_MNG dword 3. Holds SW_MNG and
/// EEP for the whole exchange.
pub fn nvm_word<Io: Registers>(io: &mut Io, port: Port, word: u16) -> R<u16, Io::Error> {
    sync::locked(io, port, sync::SW_MNG | sync::EEP, |io| {
        let fwsts = read(io, FWSTS)?;
        write(io, FWSTS, fwsts | FWSTS_FWRI)?;
        let hicr = read(io, HICR)?;
        if hicr & HICR_EN == 0 { return Err(Error::HostInterface { hicr }); }
        // Byte address, big-endian; length 2 bytes, big-endian.
        let block = [READ_SHADOW_RAM, (word as u32 * 2).swap_bytes(), 0x0000_0200, 0];
        for (i, v) in block.iter().enumerate() { write(io, FLEX_MNG + 4 * i as u32, *v)?; }
        write(io, HICR, hicr | HICR_C)?;
        let mut last = hicr | HICR_C;
        for _ in 0..HI_TIMEOUT_US / 10 {
            last = read(io, HICR)?;
            if last & HICR_C == 0 { break; }
            io.delay_us(10);
        }
        if last & HICR_C != 0 || last & HICR_SV == 0 { return Err(Error::HostInterface { hicr: last }); }
        // The data dword's upper half is padding, so all-ones is not a
        // removed device here: read it raw.
        Ok(io.read(FLEX_MNG + 12).map_err(Error::Io)? as u16)
    })
}

/// The crosstalk fix (spec 4.4, 11.4): NVM word 0x2C bit 7 clear. None when the
/// host interface or its semaphore was unavailable; the fix is then off.
pub(super) fn crosstalk_fix<Io: Registers>(io: &mut Io, port: Port) -> R<Option<bool>, Io::Error> {
    match nvm_word(io, port, 0x2c) {
        Ok(caps) => Ok(Some(caps & 0x80 == 0)),
        Err(Error::HostInterface { .. } | Error::Semaphore { .. }) => Ok(None),
        Err(e) => Err(e),
    }
}

// ---- Per-device ------------------------------------------------------------

/// Which internal (MAC-to-X557) link the board straps (NW_MNG_IF_SEL bit 24).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Internal {
    /// iXFI, forced to the copper speed (INT_PHY_MODE = 0).
    Ixfi,
    /// KR auto-negotiation (INT_PHY_MODE = 1).
    Kr,
}

/// What was done before the MAC reset (spec 9.8 steps 1–3, 9.9 steps 1–4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prepared {
    Nothing,
    Sfp { module: Module, cs4227_reset: bool },
    /// `reset` is false when the veto kept the X557 from being reset.
    Copper { phy: u8, id: u32, sel: u32, unstalled: bool, reset: bool, internal: Internal },
}

pub fn prepare<Io: Registers>(io: &mut Io, port: Port, veto: bool) -> R<Prepared, Io::Error> {
    match port.device {
        0x15ac => {
            setup_mux_ctl(io, port.lan)?;
            let cs4227_reset = check_cs4227(io, port)?;
            let module = sfp::identify(io, port)?;
            Ok(Prepared::Sfp { module, cs4227_reset })
        }
        0x15ad => prepare_copper(io, port, veto),
        _ => Ok(Prepared::Nothing),
    }
}

pub(super) fn slow_mdio<Io: Registers>(io: &mut Io) -> R<(), Io::Error> {
    let hlreg0 = read(io, HLREG0)?;
    write(io, HLREG0, hlreg0 & !MDCSPD)
}

/// Also the X553 10G_T (spec 12.12), whose MDIO accesses take the PHY token
/// and whose internal link is always KR.
pub(super) fn prepare_copper<Io: Registers>(io: &mut Io, port: Port, veto: bool) -> R<Prepared, Io::Error> {
    // Spec 2.4: the slow MDIO clock before the first access.
    slow_mdio(io)?;
    // Spec 2.5: only NW_MNG_IF_SEL's address when it is set, else a scan.
    let sel = read(io, NW_MNG_IF_SEL)?;
    let (phy, id) = if sel != 0 {
        let phy = ((sel >> 3) & 0x1f) as u8;
        (phy, mdio::probe(io, port, phy)?.ok_or(Error::NoPhy)?)
    } else {
        mdio::scan(io, port)?.ok_or(Error::NoPhy)?
    };
    // Spec 7.8.2: first start since power-on, the PHY firmware waits,
    // stalled, for software to release it.
    let alarms = mdio::read_reg(io, port, phy, mdio::PMA, PMA_TX_VENDOR_ALARMS_3)?;
    let unstalled = alarms & 3 != 0;
    if unstalled {
        mdio::modify(io, port, phy, mdio::VENDOR, VENDOR_GLOBAL_RES_PR_10, |v| v & !POWER_UP_STALL)?;
    }
    // Spec 2.6, X557 variant, unless vetoed. The LASI alarm enables of 7.8.3
    // are for interrupts; this driver polls, so they are not set.
    if !veto { mdio::reset(io, port, phy, true)?; }
    // NW_MNG_IF_SEL.INT_PHY_MODE is an X552 field; the X553 is always KR.
    let internal = if port.family == Family::X553 || sel & INT_PHY_MODE != 0 { Internal::Kr } else { Internal::Ixfi };
    Ok(Prepared::Copper { phy, id, sel, unstalled, reset: !veto, internal })
}

/// The SW_FW_SYNC bits the MAC reset holds (spec 1.4.4, 7.11).
pub fn reset_mask(port: Port) -> u32 {
    match port.device {
        0x15ad => sync::phy(port.lan),
        0x15ac => sync::SHARED_I2C,
        _ => 0,
    }
}

/// After the MAC reset (spec 7.11 step 6).
pub fn after_reset<Io: Registers>(io: &mut Io, port: Port) -> R<(), Io::Error> {
    match port.device {
        0x15ad => slow_mdio(io),
        0x15ac => setup_mux_ctl(io, port.lan),
        _ => Ok(()),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Setup {
    /// 15aa: the KX4 link is run by the hardware; nothing written.
    Kx4,
    /// 15b0: the XFI link is run by the hardware; nothing written.
    Xfi,
    /// 15ae: the external 1G PHY is run by firmware; nothing written.
    FirmwarePhy,
    /// 15ab: KR PHY auto-negotiating KR and KX (KRM LINK_CTRL_1 as written).
    Kr { link_ctrl: u32 },
    /// 15ab: manageability firmware has vetoed link changes (MMNGC.MNG_VETO).
    ManageabilityVeto,
    /// 15ac: `module` found. For a supported module, the KR PHY and the
    /// CS4227 line side (`edc`: CX1 or SR) were set for the last speed tried,
    /// and `speed` is the speed link came up at while trying (multispeed).
    /// `crosstalk`: the NVM's crosstalk fix (spec 4.4), None if the word
    /// could not be read (or no supported module, so not read).
    Sfp {
        module: Module, cs4227_reset: bool, link_ctrl: Option<u32>, edc: Option<u16>,
        speed: Option<u32>, rate_select: bool, crosstalk: Option<bool>,
    },
    /// 15ad: X557 at MDIO `phy`, advertising `advertised`.
    Copper {
        phy: u8, id: u32, sel: u32, unstalled: bool, reset: bool, internal: Internal,
        advertised: Speeds, restarted: bool,
    },
}

/// X552 link setup after the MAC reset (spec 9.6–9.9).
pub fn setup<Io: Registers>(io: &mut Io, port: Port, veto: bool, prepared: Prepared) -> R<Setup, Io::Error> {
    match (port.device, prepared) {
        (0x15ab, _) => {
            // The veto was read before the reset; read it again, it is cheap.
            if veto || read(io, MMNGC)? & MNG_VETO != 0 { return Ok(Setup::ManageabilityVeto); }
            Ok(Setup::Kr { link_ctrl: kr_autoneg(io, port, true, true)? })
        }
        (_, Prepared::Sfp { module, cs4227_reset }) => sfp_link(io, port, module, cs4227_reset),
        (_, Prepared::Copper { phy, id, sel, unstalled, reset, internal }) => {
            copper_link(io, port, veto, phy, id, sel, unstalled, reset, internal)
        }
        (0x15ae, _) => Ok(Setup::FirmwarePhy),
        (0x15b0, _) => Ok(Setup::Xfi),
        _ => Ok(Setup::Kx4),
    }
}

/// One speed on the SFP device (spec 7.7.6): the KR PHY advertising only
/// that speed (no veto check on this path), then the CS4227 EDC mode, line
/// side, at 0x12B0 + (lan << 12): (EDC << 1) | 1.
fn sfp_speed<Io: Registers>(io: &mut Io, port: Port, module: Module, ten: bool) -> R<(u32, u16), Io::Error> {
    let link_ctrl = kr_autoneg(io, port, ten, !ten)?;
    let edc = if module.linear() { CS_EDC_CX1 } else { CS_EDC_SR };
    let reg = CS_LINE_SPARE24_LSB + ((port.lan as u16) << 12);
    i2c::write_combined(io, port, CS4227, reg, (edc << 1) | 1, true)?;
    Ok((link_ctrl, edc))
}

fn links_up<Io: Registers>(io: &mut Io, port: Port, crosstalk: bool) -> R<bool, Io::Error> {
    Ok(super::cage_link(io, port.family, crosstalk)? != Link::Down)
}

/// Link speeds for an SFP+ module on the X552/X553 (spec 7.7.5, 12.11) with
/// `step` setting up one speed (`true` for 10G). 1G modules 1G only, others
/// 10G plus 1G if multispeed. Returns the last step's result, the speed
/// link came up at while trying (multispeed), and whether every soft rate
/// select succeeded.
pub(super) fn sfp_speeds<Io: Registers, T>(io: &mut Io, port: Port, module: Module, cage: bool,
    mut step: impl FnMut(&mut Io, bool) -> R<T, Io::Error>) -> R<(T, Option<u32>, bool), Io::Error> {
    if !module.multispeed {
        return Ok((step(io, !module.one_gig())?, None, true));
    }
    // Spec 5.8 with soft rate select and no laser flap: 10G, then 1G, then
    // 10G again where the port is left waiting. (A multispeed module is
    // never a 1G-only type, so both speeds apply.)
    let mut rate_ok = true;
    let mut last = None;
    for ten in [true, false, true] {
        rate_ok &= sfp::soft_rate_select(io, port, ten)?;
        delay_ms(io, 40);
        last = Some(step(io, ten)?);
        let polls = if ten { 10 } else { 1 };
        for _ in 0..polls {
            delay_ms(io, 100);
            if links_up(io, port, cage)? {
                return Ok((last.unwrap(), Some(if ten { 10_000 } else { 1000 }), rate_ok));
            }
        }
    }
    Ok((last.unwrap(), None, rate_ok))
}

fn sfp_link<Io: Registers>(io: &mut Io, port: Port, module: Module, cs4227_reset: bool) -> R<Setup, Io::Error> {
    let mut setup = Setup::Sfp {
        module, cs4227_reset, link_ctrl: None, edc: None, speed: None, rate_select: true, crosstalk: None,
    };
    if !module.supported(Family::X552) { return Ok(setup); }
    let fix = crosstalk_fix(io, port)?;
    let (last, speed, rate_ok) = sfp_speeds(io, port, module, fix == Some(true),
        |io, ten| sfp_speed(io, port, module, ten))?;
    if let Setup::Sfp { link_ctrl, edc, speed: sp, rate_select, crosstalk, .. } = &mut setup {
        *link_ctrl = Some(last.0);
        *edc = Some(last.1);
        *sp = speed;
        *rate_select = rate_ok;
        *crosstalk = fix;
    }
    Ok(setup)
}

/// Spec 7.8.5: iXFI forced to 10G (1 s for the internal link), then the X557
/// advertises 10G + 1G (1.0x0004 without 100M) and restarts AN unless vetoed.
#[allow(clippy::too_many_arguments)]
fn copper_link<Io: Registers>(io: &mut Io, port: Port, veto: bool, phy: u8, id: u32, sel: u32,
    unstalled: bool, reset: bool, internal: Internal) -> R<Setup, Io::Error> {
    let mut s = mdio::abilities(io, port, phy)?;
    s.m100 = false;
    if internal == Internal::Ixfi {
        ixfi(io, port, s.g10)?;
        for _ in 0..10 {
            delay_ms(io, 100);
            if read(io, LINKS)? & LINK_UP != 0 && mdio::an_link(io, port, phy)? { break; }
        }
    }
    let restarted = mdio::advertise(io, port, phy, s, None, veto)?;
    Ok(Setup::Copper { phy, id, sel, unstalled, reset, internal, advertised: s, restarted })
}

/// The copper side, polled (spec 7.8.6, 12.12).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Copper {
    Down,
    Up { megabits: u32 },
    /// Up at a speed the internal link cannot carry (7.0xC800 bits 2:0).
    Invalid { status: u16 },
}

/// Follows the X557 while `hardware::wait_link` polls: on each copper
/// link-up or speed change, the internal link is re-forced to the copper
/// speed (KR mode: KR AN 10G + 1G), as the shared code does on the X557's
/// link alarm.
pub struct Watch { phy: u8, internal: Internal, forced: Option<u16>, pub reforced: u32 }

impl Watch {
    pub fn new(phy: u8, internal: Internal) -> Self { Watch { phy, internal, forced: None, reforced: 0 } }

    pub fn poll<Io: Registers>(&mut self, io: &mut Io, port: Port) -> R<Copper, Io::Error> {
        if !mdio::an_link(io, port, self.phy)? { self.forced = None; return Ok(Copper::Down); }
        let status = mdio::read_reg(io, port, self.phy, mdio::AN, AN_VENDOR_STATUS)? & 7;
        if !mdio::an_link(io, port, self.phy)? { self.forced = None; return Ok(Copper::Down); }
        let megabits = match status {
            7 => 10_000,
            5 => 1000,
            _ => return Ok(Copper::Invalid { status }),
        };
        if self.forced != Some(status) {
            match self.internal {
                Internal::Kr => { kr_autoneg(io, port, true, true)?; }
                Internal::Ixfi => { ixfi(io, port, megabits == 10_000)?; }
            }
            self.forced = Some(status);
            self.reforced += 1;
        }
        Ok(Copper::Up { megabits })
    }
}
