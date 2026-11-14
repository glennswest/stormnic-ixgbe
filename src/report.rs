//! The stormnic contract (#35, owner 2026-11-14): the driver brings its
//! port up itself with the IEEE 802.3 standard FEC for its media and speed,
//! and reports one line per port: driver, PCI address, MAC, media, speed,
//! FEC and link. A port that did not reach its standard FEC or link is an
//! `error:` line, never a quiet one. Nothing is handed down from stormbootx.
//!
//! The standard FEC for every speed and media this family runs is **none**:
//! 10GBASE-SR/LR/ER, SFP+ direct attach (SFF-8431 SFI), 10GBASE-KX4, XAUI,
//! CX4, XFI, 1000BASE-X/KX, SGMII, 2.5G backplane and 10M..1G copper have no
//! FEC in 802.3; 10GBASE-T and 2.5G/5GBASE-T carry their LDPC inside the
//! PMA, which is not a selectable FEC; and clause-74 BASE-R FEC on
//! 10GBASE-KR is optional, negotiated only in clause-73 AN. So the one place
//! a FEC could run is KR AN, and link setup clears its request and ability
//! bits there (82599 AUTOC FECR/FECA, X552/X553 KRM LINK_CTRL_1
//! AN_FEC_REQ/AN_CAP_FEC). `fec` reads them back after link setup: what is
//! read is what runs.

use super::f82599::{self, FECA, FECR};
use super::sfp::{Kind, Module};
use super::x552::{self, LC1_CAP_FEC, LC1_FEC_REQ};
use super::x553;
use super::{link_mode, read, Error, Family, Link, Port, Registers, Setup, AUTOC, AUTOC2, R};
use core::fmt;

/// The FEC on the line after link setup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fec {
    /// No FEC: the standard for this media and speed, and what runs.
    None,
    /// KR AN still requests or offers clause-74 FEC (a manageability veto
    /// kept the driver's write out): `register` names AUTOC or LINK_CTRL_1.
    Negotiable { register: &'static str, value: u32 },
    /// The KR register could not be read back.
    Unread,
}

impl Fec {
    /// Whether the port runs its standard FEC.
    pub fn standard(self) -> bool { self == Fec::None }
}

/// 82599 AUTOC link modes with clause-73 KR AN (spec 5.1 LMS 4, 6, 7).
fn kr_an(autoc: u32) -> bool { matches!((autoc >> 13) & 7, 4 | 6 | 7) }

/// Whether this port's link setup goes through KR AN on the X552/X553 KR
/// PHY (spec 7.4): 15ab KR, 15ac SFP+ (KR to the CS4227), X553 15c2/15c3 KR
/// (not when strapped for 2.5G backplane) and 15ce SFP+ (KR to the retimer).
fn x55x_kr(port: Port, setup: Option<&Setup>) -> bool {
    match (port.family, setup) {
        (Family::X552, Some(Setup::X552(x552::Setup::Sfp { link_ctrl, .. }))) => link_ctrl.is_some(),
        (Family::X552, _) => port.device == 0x15ab,
        (Family::X553, Some(Setup::X553(x553::Setup::Kr2500 { .. }))) => false,
        (Family::X553, Some(Setup::X553(x553::Setup::Sfp { sfi, .. }))) => {
            matches!(sfi, Some(x553::Sfi::Retimer { .. }))
        }
        (Family::X553, _) => matches!(port.device, 0x15c2 | 0x15c3),
        _ => false,
    }
}

/// The FEC that runs, read back after link setup (see the module comment).
/// `setup` is None when link setup was skipped or failed.
pub fn fec<Io: Registers>(io: &mut Io, port: Port, setup: Option<&Setup>) -> R<Fec, Io::Error> {
    match port.family {
        Family::F82599 => {
            let autoc = read(io, AUTOC)?;
            Ok(if kr_an(autoc) && autoc & (FECR | FECA) != 0 {
                Fec::Negotiable { register: "AUTOC", value: autoc }
            } else {
                Fec::None
            })
        }
        Family::X552 | Family::X553 if x55x_kr(port, setup) => {
            let lc1 = match x552::kr_read(io, port, x552::krm(port.lan, x552::KRM_LINK_CTRL_1)) {
                Ok(v) => v,
                Err(Error::Io(e)) => return Err(Error::Io(e)),
                Err(Error::Removed) => return Err(Error::Removed),
                Err(_) => return Ok(Fec::Unread),
            };
            Ok(if lc1 & x552::LC1_AN_ENABLE != 0 && lc1 & (LC1_FEC_REQ | LC1_CAP_FEC) != 0 {
                Fec::Negotiable { register: "LINK_CTRL_1", value: lc1 }
            } else {
                Fec::None
            })
        }
        _ => Ok(Fec::None),
    }
}

/// The media a port runs on, for the report line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Media {
    /// An SFP+ (or QSFP+ when `qsfp`) cage and the module found in it.
    Cage { qsfp: bool, module: Option<Module> },
    /// 82599 backplane / CX4 / LCO: AUTOC's link mode.
    Backplane { autoc: u32, autoc2: u32 },
    Named(&'static str),
}

/// The media from the device ID and what link setup found. `autoc` is
/// AUTOC/AUTOC2 as they are now (82599 only; the others ignore it).
pub fn media(port: Port, setup: Option<&Setup>, autoc: (u32, u32)) -> Media {
    let module = |m: &Module| Some(*m);
    match port.family {
        Family::F82599 => match (f82599::media(port.device), setup) {
            (f82599::Media::Copper, _) => Media::Named("10GBASE-T (TN1010 PHY)"),
            (f82599::Media::Lco, _) => Media::Named("fiber LCO"),
            (f82599::Media::Backplane, _) => Media::Backplane { autoc: autoc.0, autoc2: autoc.1 },
            (m, Some(Setup::F82599(f82599::Setup::Module { module: ref md, .. }))) => {
                Media::Cage { qsfp: m == f82599::Media::Qsfp, module: module(md) }
            }
            (m, _) => Media::Cage { qsfp: m == f82599::Media::Qsfp, module: None },
        },
        Family::X540 => Media::Named("10GBASE-T"),
        Family::X550 => Media::Named("10GBASE-T (NBASE-T 5G/2.5G)"),
        Family::X552 => match (port.device, setup) {
            (0x15aa, _) => Media::Named("backplane 10GBASE-KX4"),
            (0x15ab, _) => Media::Named("backplane 10GBASE-KR/1000BASE-KX"),
            (0x15ac, Some(Setup::X552(x552::Setup::Sfp { module: ref m, .. }))) => {
                Media::Cage { qsfp: false, module: module(m) }
            }
            (0x15ac, _) => Media::Cage { qsfp: false, module: None },
            (0x15ad, _) => Media::Named("10GBASE-T (X557 PHY)"),
            (0x15ae, _) => Media::Named("1000BASE-T (external PHY)"),
            _ => Media::Named("XFI"),
        },
        Family::X553 => match (port.device, setup) {
            (_, Some(Setup::X553(x553::Setup::Kr2500 { .. }))) => Media::Named("backplane 2.5G"),
            (0x15c2, _) => Media::Named("backplane 10GBASE-KR/1000BASE-KX"),
            (0x15c3, _) => Media::Named("backplane 1000BASE-KX"),
            (0x15c6 | 0x15c7, _) => Media::Named("SGMII 1G"),
            (0x15c8, _) => Media::Named("10GBASE-T (X557 PHY)"),
            (0x15e4 | 0x15e5, _) => Media::Named("1000BASE-T (firmware PHY)"),
            (_, Some(Setup::X553(x553::Setup::Sfp { module: ref m, .. }))) => {
                Media::Cage { qsfp: false, module: module(m) }
            }
            _ => Media::Cage { qsfp: false, module: None },
        },
    }
}

impl fmt::Display for Media {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Media::Named(n) => f.write_str(n),
            Media::Backplane { autoc, autoc2 } => write!(f, "backplane {}", link_mode(autoc, autoc2)),
            Media::Cage { qsfp, module } => {
                f.write_str(if qsfp { "QSFP+ " } else { "SFP+ " })?;
                f.write_str(match module.map(|m| m.kind) {
                    None => "module not read",
                    Some(Kind::DaCu) => "passive DA",
                    Some(Kind::DaActiveLimiting) => "active DA",
                    Some(Kind::SrLr) => "10GBASE-SR/LR",
                    Some(Kind::Cu1g) => "1000BASE-T",
                    Some(Kind::Sx1g) => "1000BASE-SX",
                    Some(Kind::Lx1g) => "1000BASE-LX",
                    Some(Kind::Bx10g) => "10G BX",
                    Some(Kind::Bx1g) => "1000BASE-BX",
                    Some(Kind::Unknown) => "unknown module",
                    Some(Kind::NotPresent) => "cage empty",
                })
            }
        }
    }
}

/// The one line a port reports at bind (#22, #35). `at` is the PCI address,
/// `down` is what to say after "no link" when the link is down (how long
/// Start waited and the registers).
pub struct PortLine<'a, A: fmt::Display, D: fmt::Display> {
    pub version: &'a str,
    pub at: A,
    pub device: u16,
    pub name: &'a str,
    pub mac: [u8; 6],
    pub media: Media,
    pub link: Link,
    pub fec: Fec,
    pub down: D,
}

impl<A: fmt::Display, D: fmt::Display> PortLine<'_, A, D> {
    /// Whether the line is an error (no link, or not the standard FEC).
    pub fn error(&self) -> bool { self.link == Link::Down || !self.fec.standard() }
}

impl<A: fmt::Display, D: fmt::Display> fmt::Display for PortLine<'_, A, D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let m = self.mac;
        write!(
            f,
            "stormnic-ixgbe {}: {} 8086:{:04x} {}: MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}, media {}, ",
            self.version, self.at, self.device, self.name, m[0], m[1], m[2], m[3], m[4], m[5], self.media
        )?;
        match self.link {
            Link::Up { megabits: Some(mb) } => write!(f, "speed {mb} Mb/s full duplex, ")?,
            Link::Up { megabits: None } => f.write_str("speed unknown (reserved LINKS encoding), ")?,
            Link::Down => f.write_str("speed none, ")?,
        }
        match self.fec {
            Fec::None => f.write_str("FEC none, ")?,
            Fec::Negotiable { register, value } => write!(
                f,
                "FEC not set: error: KR AN still requests or offers clause-74 FEC ({register} {value:08x}; manageability veto), "
            )?,
            Fec::Unread => f.write_str("FEC unread: error: KR LINK_CTRL_1 could not be read back, ")?,
        }
        match self.link {
            Link::Up { .. } => f.write_str("link UP"),
            Link::Down => write!(f, "link DOWN: error: no link {}", self.down),
        }
    }
}

/// AUTOC and AUTOC2 for `media` (82599; 0 for the others, which ignore it).
pub fn autoc<Io: Registers>(io: &mut Io, family: Family) -> R<(u32, u32), Io::Error> {
    if family != Family::F82599 { return Ok((0, 0)); }
    Ok((read(io, AUTOC)?, read(io, AUTOC2)?))
}
