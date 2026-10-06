//! X553 (Atom C3000 integrated 10 GbE, the shared code's X550EM_a) link
//! setup, per device ID (spec 12).
//!
//! - 15c2 KR and 15c3 KR "L" (1G only) backplane: KR AN through the IOSF
//!   sideband as on the X552, plus the X553's PMD_FLX_MASK_ST20 lane mode
//!   and firmware AN-restart flag (`x552::kr_autoneg`). A board strapped
//!   for 2.5G backplane is left alone.
//! - 15c6, 15c7 SGMII: the internal PHY set to SGMII at 1G.
//! - 15e4, 15e5 1G copper: the PHY belongs to the firmware; it is reset and
//!   set up through firmware PHY activities (`hostif`), and the internal PHY
//!   runs SGMII with AN.
//! - 15c8 10GBASE-T: the X552's X557 path (`x552`), with MDIO under the
//!   firmware's PHY token and the internal link always KR.
//! - 15c4 SFP+ "N": native SFI, no retimer. 15ce SFP+: KR to a CS4227 or
//!   CS4223 retimer reached on MDIO. Both identify the module over I2C and
//!   run the X552's multispeed loop.

use super::mdio;
use super::sfp::{self, Module};
use super::x552::{self, kr_modify, kr_write, krm, restart_an, FLX_AN, FLX_AN37, FLX_SFI_MODE, FLX_SFI_SR,
    FLX_SGMII, FLX_SPEED, FLX_SPEED_10G, FLX_SPEED_1G, FLX_SPEED_AN, KRM_FLX, KRM_LINK_CTRL_1, LC1_AN_ENABLE,
    LC1_CAP_KR, LC1_CLAUSE_37, LC1_FORCE_1G, LC1_FORCE_SPEED, LC1_SGMII};
use super::{hostif, read, sync, write, Error, Family, Port, Registers, HLREG0, MMNGC, R};

const NW_MNG_IF_SEL: u32 = 0x11178;
const SEL_MDIO_ACT: u32 = 1 << 1;
const SEL_SPEED_2_5G: u32 = 1 << 20;
const MDCSPD: u32 = 1 << 16;
const MNG_VETO: u32 = 1 << 0;

const KRM_SGMII_CTRL: u32 = 0x42a0;
const SGMII_FORCE_100: u32 = 1 << 12;
const SGMII_FORCE_10: u32 = 1 << 19;

// Firmware PHY activity fields (spec 12.6).
const INFO_SPEEDS: u32 = 0xfff;
const SETUP_HP: u32 = 1 << 19;
const SETUP_AN: u32 = 1 << 22;
const SETUP_DOWN: u32 = 1 << 0;
const LINK_INFO_TEMP: u32 = 1 << 25;
const FORCE_DOWN_OFF: u32 = 1 << 0;

// External retimer on 15ce (spec 12.11).
const CS_EFUSE_PDF_SKU: u16 = 0x019f;
const CS4223_SKU: u16 = 0x0010;
const CS_LINE_SPARE24_LSB: u16 = 0x12b0;
const CS_EDC_CX1: u16 = 0x0002;
const CS_EDC_SR: u16 = 0x0004;
const NVM_CTRL_4: u16 = 0x45;
const NVM_INSTANCE: u16 = 1 << 4;

/// Copper devices: the 9 s link budget (spec 8.1).
pub fn copper(device: u16) -> bool { matches!(device, 0x15c8 | 0x15e4 | 0x15e5) }

fn fiber(device: u16) -> bool { matches!(device, 0x15c4 | 0x15ce) }

fn firmware_phy(device: u16) -> bool { matches!(device, 0x15e4 | 0x15e5) }

/// The MDIO clock before the first access and after the MAC reset (spec
/// 12.9–12.12): slow for SGMII, 10G_T and 15ce, fast for the firmware PHY.
fn set_mdio_speed<Io: Registers>(io: &mut Io, device: u16) -> R<(), Io::Error> {
    let fast = match device {
        0x15c6 | 0x15c7 | 0x15c8 | 0x15ce => false,
        0x15e4 | 0x15e5 => true,
        _ => return Ok(()),
    };
    let hlreg0 = read(io, HLREG0)?;
    write(io, HLREG0, if fast { hlreg0 | MDCSPD } else { hlreg0 & !MDCSPD })
}

/// What was done before the MAC reset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prepared {
    Nothing,
    Sfp { module: Module },
    /// 15c8: the X557, as on the X552 (`x552::Prepared::Copper`).
    X557(x552::Prepared),
    /// 15e4/15e5: GET_PHY_INFO's PHY ID and speed bits; `reset` is false
    /// when the veto kept the PHY from being reset and set up. `down`: the
    /// reset's SETUP_LINK answered that the PHY is down (over-temperature).
    Firmware { id: u32, speeds: u16, reset: bool, down: bool },
}

pub fn prepare<Io: Registers>(io: &mut Io, port: Port, veto: bool) -> R<Prepared, Io::Error> {
    set_mdio_speed(io, port.device)?;
    match port.device {
        d if fiber(d) => {
            x552::setup_mux_ctl(io, port.lan)?;
            Ok(Prepared::Sfp { module: sfp::identify(io, port)? })
        }
        0x15c8 => Ok(Prepared::X557(x552::prepare_copper(io, port, veto)?)),
        d if firmware_phy(d) => prepare_firmware(io, port, veto),
        _ => Ok(Prepared::Nothing),
    }
}

/// Spec 12.10 steps 1–2: identify, then (unless vetoed) reset, initialise
/// and set up the PHY through the firmware.
fn prepare_firmware<Io: Registers>(io: &mut Io, port: Port, veto: bool) -> R<Prepared, Io::Error> {
    let info = hostif::phy_activity(io, port, hostif::GET_PHY_INFO, [0; 4])?;
    let speeds = (info[0] & INFO_SPEEDS) as u16;
    let id = (info[0] & 0xffff_0000) | (info[1] & 0xfff0);
    if id == 0 || id == 0xffff_fff0 { return Err(Error::NoPhy); }
    if veto { return Ok(Prepared::Firmware { id, speeds, reset: false, down: false }); }
    hostif::phy_activity(io, port, hostif::PHY_SW_RESET, [0; 4])?;
    hostif::phy_activity(io, port, hostif::INIT_PHY, [0; 4])?;
    let down = setup_fw_link(io, port, speeds)?;
    Ok(Prepared::Firmware { id, speeds, reset: true, down })
}

/// SETUP_LINK with the PHY's own speeds, high power and AN; no pause and no
/// EEE (spec 12.10 step 4). True when the firmware answers that the PHY is
/// down (over-temperature).
fn setup_fw_link<Io: Registers>(io: &mut Io, port: Port, speeds: u16) -> R<bool, Io::Error> {
    let out = hostif::phy_activity(io, port, hostif::SETUP_LINK, [speeds as u32 | SETUP_HP | SETUP_AN, 0, 0, 0])?;
    Ok(out[0] == SETUP_DOWN)
}

/// The SW_FW_SYNC bits the MAC reset holds (spec 12.13).
pub fn reset_mask(port: Port) -> u32 {
    match port.device {
        d if fiber(d) => sync::SHARED_I2C,
        d if copper(d) => sync::phy(port.lan),
        _ => 0,
    }
}

/// After the MAC reset: the MDIO clock again (spec 12.13).
pub fn after_reset<Io: Registers>(io: &mut Io, port: Port) -> R<(), Io::Error> {
    set_mdio_speed(io, port.device)
}

/// One SFP speed step's result (spec 12.11).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sfi {
    /// 15c4: PMD_FLX_MASK_ST20 as written for native SFI.
    Native { flx: u32 },
    /// 15ce: KR AN to the retimer (LINK_CTRL_1 as written), the retimer's
    /// SKU word, the line-side register written and the EDC mode (CX1 or SR).
    Retimer { link_ctrl: u32, sku: u16, register: u16, edc: u16 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Setup {
    /// 15c2/15c3: KR AN advertising KR (unless 15c3) and KX.
    Kr { link_ctrl: u32 },
    /// 15c2/15c3: NW_MNG_IF_SEL selects 2.5G backplane; left alone.
    Kr2500 { sel: u32 },
    /// 15c6/15c7: internal PHY in SGMII at 1G.
    Sgmii { link_ctrl: u32, sgmii: u32, flx: u32 },
    /// Manageability firmware has vetoed link changes (MMNGC.MNG_VETO).
    ManageabilityVeto,
    /// 15c4/15ce: `module`, and for a supported one the last speed step,
    /// the speed link came up at while trying (multispeed), whether every
    /// soft rate select worked, and the crosstalk fix (None: NVM word
    /// unreadable, or no supported module).
    Sfp { module: Module, sfi: Option<Sfi>, speed: Option<u32>, rate_select: bool, crosstalk: Option<bool> },
    /// 15c8: as on the X552.
    X557(x552::Setup),
    /// 15e4/15e5: the PHY's ID and speeds; `internal` the PMD_FLX_MASK_ST20
    /// written for SGMII AN (None when vetoed); `overtemp` when the firmware
    /// reported over-temperature (the link was then forced down).
    Firmware { id: u32, speeds: u16, internal: Option<u32>, overtemp: bool },
}

/// The veto was read before the reset; read it again, it is cheap.
fn vetoed<Io: Registers>(io: &mut Io, veto: bool) -> R<bool, Io::Error> {
    Ok(veto || read(io, MMNGC)? & MNG_VETO != 0)
}

pub fn setup<Io: Registers>(io: &mut Io, port: Port, veto: bool, prepared: Prepared) -> R<Setup, Io::Error> {
    match (port.device, prepared) {
        (0x15c2 | 0x15c3, _) => {
            let sel = read(io, NW_MNG_IF_SEL)?;
            if sel & SEL_SPEED_2_5G != 0 { return Ok(Setup::Kr2500 { sel }); }
            if vetoed(io, veto)? { return Ok(Setup::ManageabilityVeto); }
            // 15c3 (the "L" part) is 1G only (spec 12.8).
            Ok(Setup::Kr { link_ctrl: x552::kr_autoneg(io, port, port.device != 0x15c3, true)? })
        }
        (0x15c6 | 0x15c7, _) => {
            if vetoed(io, veto)? { return Ok(Setup::ManageabilityVeto); }
            sgmii(io, port)
        }
        (_, Prepared::Sfp { module }) => sfp_link(io, port, module),
        (_, Prepared::X557(p)) => Ok(Setup::X557(x552::setup(io, port, veto, p)?)),
        (_, Prepared::Firmware { id, speeds, reset, down }) => {
            if !reset || vetoed(io, veto)? { return Ok(Setup::Firmware { id, speeds, internal: None, overtemp: false }); }
            // Spec 12.10 steps 3–5; a PHY already down for temperature is
            // not set up again.
            let mut overtemp = down;
            let mut internal = None;
            if !overtemp {
                internal = Some(sgmii_an(io, port)?);
                overtemp = setup_fw_link(io, port, speeds)?;
            }
            if !overtemp {
                let info = hostif::phy_activity(io, port, hostif::GET_LINK_INFO, [0; 4])?;
                overtemp = info[0] & LINK_INFO_TEMP != 0;
            }
            if overtemp {
                hostif::phy_activity(io, port, hostif::FORCE_LINK_DOWN, [FORCE_DOWN_OFF, 0, 0, 0])?;
            }
            Ok(Setup::Firmware { id, speeds, internal, overtemp })
        }
        // Every X553 ID is matched above; anything else has no path.
        _ => Err(Error::NoPhy),
    }
}

/// SGMII at 1G (spec 12.9).
fn sgmii<Io: Registers>(io: &mut Io, port: Port) -> R<Setup, Io::Error> {
    let link_ctrl = kr_modify(io, port, KRM_LINK_CTRL_1, |v| {
        (v & !(LC1_AN_ENABLE | LC1_FORCE_SPEED)) | LC1_SGMII | LC1_CLAUSE_37 | LC1_FORCE_1G
    })?;
    let sgmii = kr_modify(io, port, KRM_SGMII_CTRL, |v| v | SGMII_FORCE_10 | SGMII_FORCE_100)?;
    let flx = kr_modify(io, port, KRM_FLX, |v| {
        (v & !(FLX_SPEED | FLX_AN)) | FLX_SPEED_1G | FLX_SGMII | FLX_AN37
    })?;
    restart_an(io, port)?;
    Ok(Setup::Sgmii { link_ctrl, sgmii, flx })
}

/// SGMII with AN to the firmware's PHY (spec 12.10 step 3). Returns
/// PMD_FLX_MASK_ST20 as written.
fn sgmii_an<Io: Registers>(io: &mut Io, port: Port) -> R<u32, Io::Error> {
    let lc1 = kr_modify(io, port, KRM_LINK_CTRL_1, |v| {
        (v & !(LC1_AN_ENABLE | LC1_FORCE_SPEED)) | LC1_SGMII | LC1_CLAUSE_37
    })?;
    kr_modify(io, port, KRM_SGMII_CTRL, |v| v & !(SGMII_FORCE_10 | SGMII_FORCE_100))?;
    kr_write(io, port, krm(port.lan, KRM_LINK_CTRL_1), lc1)?;
    let flx = kr_modify(io, port, KRM_FLX, |v| {
        (v & !(FLX_SPEED | FLX_AN)) | FLX_SPEED_AN | FLX_SGMII | FLX_AN37
    })?;
    restart_an(io, port)?;
    Ok(flx)
}

fn sfp_link<Io: Registers>(io: &mut Io, port: Port, module: Module) -> R<Setup, Io::Error> {
    if !module.supported(Family::X553) {
        return Ok(Setup::Sfp { module, sfi: None, speed: None, rate_select: true, crosstalk: None });
    }
    let crosstalk = x552::crosstalk_fix(io, port)?;
    let (sfi, speed, rate_select) = x552::sfp_speeds(io, port, module, crosstalk == Some(true),
        |io, ten| sfp_speed(io, port, module, ten))?;
    Ok(Setup::Sfp { module, sfi: Some(sfi), speed, rate_select, crosstalk })
}

/// One speed (spec 12.11): native SFI on 15c4, KR plus the retimer's EDC
/// mode on 15ce. No veto check, as on the X552.
fn sfp_speed<Io: Registers>(io: &mut Io, port: Port, module: Module, ten: bool) -> R<Sfi, Io::Error> {
    if port.device == 0x15c4 {
        kr_modify(io, port, KRM_FLX, |v| {
            let v = v & !FLX_SFI_MODE;
            if module.linear() { v } else { v | FLX_SFI_SR }
        })?;
        let flx = kr_modify(io, port, KRM_FLX, |v| {
            (v & !(FLX_AN | FLX_AN37 | FLX_SGMII | FLX_SPEED)) | if ten { FLX_SPEED_10G } else { FLX_SPEED_1G }
        })?;
        restart_an(io, port)?;
        return Ok(Sfi::Native { flx });
    }
    let link_ctrl = x552::kr_autoneg(io, port, ten, !ten)?;
    let sel = read(io, NW_MNG_IF_SEL)?;
    let phy = ((sel >> 3) & 0x1f) as u8;
    if sel & SEL_MDIO_ACT == 0 || phy == 0 { return Err(Error::NoPhy); }
    let sku = mdio::read_reg(io, port, phy, 0, CS_EFUSE_PDF_SKU)?;
    let slot = if sku == CS4223_SKU {
        // The quad-port CS4223 also counts the MAC instance (NVM 0x45 bit 4).
        let instance = (x552::nvm_word(io, port, NVM_CTRL_4)? & NVM_INSTANCE != 0) as u16;
        port.lan as u16 + (instance << 1)
    } else {
        port.lan as u16
    };
    let register = CS_LINE_SPARE24_LSB + (slot << 12);
    let edc = if module.linear() { CS_EDC_CX1 } else { CS_EDC_SR };
    let value = mdio::read_reg(io, port, phy, 0, register)?;
    let value = (value & !((CS_EDC_CX1 | CS_EDC_SR) << 1)) | (edc << 1) | 1;
    mdio::write_reg(io, port, phy, 0, register, value)?;
    mdio::read_reg(io, port, phy, 0, register)?;
    Ok(Sfi::Retimer { link_ctrl, sku, register, edc })
}

/// Whether a KR LINK_CTRL_1 advertises 10G (for the console).
pub fn advertises_kr(link_ctrl: u32) -> bool { link_ctrl & LC1_CAP_KR != 0 }
