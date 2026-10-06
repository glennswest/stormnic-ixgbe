//! SFP+ / QSFP+ module identification (spec 4) over `i2c`.

use super::{i2c, Error, Family, Port, Registers, R};

const EEPROM: u8 = 0xa0;
const DIAG: u8 = 0xa2;

// Byte 3, 10G compliance; byte 6, 1G compliance; byte 8, cable technology.
const SR: u8 = 0x10;
const LR: u8 = 0x20;
const SX: u8 = 0x01;
const LX: u8 = 0x02;
const BASE_T: u8 = 0x08;
const BX10: u8 = 0x40;
const PASSIVE_DA: u8 = 0x04;
const ACTIVE_DA: u8 = 0x08;

/// The shared code's sfp_type (spec 4.2 step 5), without the core number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Passive direct-attach copper ("linear").
    DaCu,
    /// Active direct-attach with limiting electronics.
    DaActiveLimiting,
    /// 10GBASE-SR or -LR.
    SrLr,
    /// 1000BASE-T.
    Cu1g,
    Sx1g,
    Lx1g,
    /// 10G BX (10.3 GBd, single-mode, no 10G compliance code).
    Bx10g,
    Bx1g,
    /// An identifier that is not an SFP (QSFP), or no rule matched.
    Unknown,
    /// Nothing answered.
    NotPresent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Module {
    pub kind: Kind,
    /// Tries 10G then 1G (spec 4.2 step 7).
    pub multispeed: bool,
    pub identifier: u8,
    /// SFP bytes 3, 6 and 8 (QSFP: 0x83, 0x86, 0).
    pub comp_10g: u8,
    pub comp_1g: u8,
    pub cable: u8,
}

impl Module {
    fn absent() -> Self {
        Module { kind: Kind::NotPresent, multispeed: false, identifier: 0, comp_10g: 0, comp_1g: 0, cable: 0 }
    }
    pub fn present(self) -> bool { self.kind != Kind::NotPresent }
    /// 1G module types (cu, sx, lx, bx): 1G only (spec 5.6, 7.7.5).
    pub fn one_gig(self) -> bool { matches!(self.kind, Kind::Cu1g | Kind::Sx1g | Kind::Lx1g | Kind::Bx1g) }
    /// Passive DA drives the X552 CS4227 EDC mode (spec 7.7.4).
    pub fn linear(self) -> bool { self.kind == Kind::DaCu }
    /// Support (spec 4.2 steps 9–10, 7.7.4). The Intel-OUI rule is a support
    /// policy, not a hardware limit, and a boot driver may skip it (spec 4.2
    /// boot-driver note); this one does. `unknown` is always refused (no NVM
    /// init sequence or EDC mode exists for it); the X552 also refuses 1G-T.
    pub fn supported(self, family: Family) -> bool {
        match self.kind {
            Kind::NotPresent | Kind::Unknown => false,
            Kind::Cu1g => family != Family::X552,
            _ => true,
        }
    }
    /// The 82599 NVM init-sequence key: the sfp_type value for this core,
    /// with every type but passive DA looked up as SR/LR (spec 5.5 step 1).
    pub fn nvm_key(self, lan: u8) -> Option<u16> {
        let core = lan as u16;
        match self.kind {
            Kind::DaCu => Some(3 + core),
            Kind::NotPresent | Kind::Unknown => None,
            _ => Some(5 + core),
        }
    }
}

/// A module byte; an I2C failure is None ("not present", spec 4.2 step 3).
fn byte<Io: Registers>(io: &mut Io, port: Port, device: u8, offset: u8, attempts: usize)
    -> R<Option<u8>, Io::Error> {
    match i2c::read_byte(io, port, device, offset, attempts, true) {
        Ok(v) => Ok(Some(v)),
        Err(Error::I2c { .. }) => Ok(None),
        Err(e) => Err(e),
    }
}

macro_rules! or_absent {
    ($e:expr) => { match $e? { Some(v) => v, None => return Ok(Module::absent()) } };
}

/// SFF-8472 identification (spec 4.2).
pub fn identify<Io: Registers>(io: &mut Io, port: Port) -> R<Module, Io::Error> {
    let short = i2c::attempts(port);
    // Step 1: some modules ACK before their data is ready, so a wrong value
    // is read again, up to 5 times, with the long probe retry count.
    let mut identifier = 0;
    for _ in 0..5 {
        identifier = or_absent!(byte(io, port, EEPROM, 0, 11));
        if identifier == 0x03 { break; }
    }
    let mut m = Module { identifier, ..Module::absent() };
    if identifier != 0x03 { m.kind = Kind::Unknown; return Ok(m); }
    m.comp_1g = or_absent!(byte(io, port, EEPROM, 6, short));
    m.comp_10g = or_absent!(byte(io, port, EEPROM, 3, short));
    m.cable = or_absent!(byte(io, port, EEPROM, 8, short));
    let (g1, g10, cable) = (m.comp_1g, m.comp_10g, m.cable);
    // Step 4: 10G-BX, by nominal rate and single-mode reach.
    let mut bx10g = false;
    if g10 == 0 && g1 & (SX | LX | BASE_T) == 0 && cable & (PASSIVE_DA | ACTIVE_DA) == 0 {
        if or_absent!(byte(io, port, EEPROM, 12, short)) == 0x67 {
            let km = or_absent!(byte(io, port, EEPROM, 14, short));
            let hm = or_absent!(byte(io, port, EEPROM, 15, short));
            bx10g = km > 0 || hm >= 10;
        }
    }
    // Step 5: the first matching row wins.
    m.kind = if cable & PASSIVE_DA != 0 {
        Kind::DaCu
    } else if cable & ACTIVE_DA != 0 {
        if or_absent!(byte(io, port, EEPROM, 60, short)) & 0x04 != 0 { Kind::DaActiveLimiting } else { Kind::Unknown }
    } else if g10 & (SR | LR) != 0 {
        Kind::SrLr
    } else if g1 & BASE_T != 0 {
        Kind::Cu1g
    } else if g1 & SX != 0 {
        Kind::Sx1g
    } else if g1 & LX != 0 {
        Kind::Lx1g
    } else if bx10g {
        Kind::Bx10g
    } else if g1 & BX10 != 0 {
        Kind::Bx1g
    } else {
        Kind::Unknown
    };
    // Step 7.
    m.multispeed = (g1 & SX != 0 && g10 & SR != 0) || (g1 & LX != 0 && g10 & LR != 0)
        || matches!(m.kind, Kind::DaCu | Kind::DaActiveLimiting);
    Ok(m)
}

/// QSFP+ identification (spec 4.5): identifier 0x0D, 10G compliance at 0x83,
/// 1G at 0x86. Multispeed is 1G SX with 10G SR, or 1G LX with 10G LR; unlike
/// SFP+, a DA cable is not.
pub fn identify_qsfp<Io: Registers>(io: &mut Io, port: Port) -> R<Module, Io::Error> {
    let short = i2c::attempts(port);
    let identifier = or_absent!(byte(io, port, EEPROM, 0, 11));
    let mut m = Module { identifier, ..Module::absent() };
    if identifier != 0x0d { m.kind = Kind::Unknown; return Ok(m); }
    m.comp_10g = or_absent!(byte(io, port, EEPROM, 0x83, short));
    m.comp_1g = or_absent!(byte(io, port, EEPROM, 0x86, short));
    let g10 = m.comp_10g;
    m.kind = if g10 & 0x08 != 0 {
        Kind::DaCu
    } else if g10 & (SR | LR) != 0 {
        Kind::SrLr
    } else if g10 & 0x01 != 0 {
        Kind::DaActiveLimiting
    } else if or_absent!(byte(io, port, EEPROM, 0x82, short)) == 0x23
        && or_absent!(byte(io, port, EEPROM, 0x92, short)) > 0
        && or_absent!(byte(io, port, EEPROM, 0x93, short)) >> 4 == 0 {
        Kind::DaActiveLimiting
    } else {
        Kind::Unknown
    };
    let g1 = m.comp_1g;
    m.multispeed = (g1 & SX != 0 && g10 & SR != 0) || (g1 & LX != 0 && g10 & LR != 0);
    Ok(m)
}

/// Soft rate select (spec 4.3): A2:0x6E bit 3 (RS0) and A2:0x76 bit 3 (RS1),
/// set for 10G, clear for 1G. An I2C failure is not an error; the result
/// says whether both bytes were written.
pub fn soft_rate_select<Io: Registers>(io: &mut Io, port: Port, ten: bool) -> R<bool, Io::Error> {
    let mut ok = true;
    for offset in [0x6e, 0x76] {
        let Some(v) = byte(io, port, DIAG, offset, i2c::attempts(port))? else { ok = false; continue };
        let v = if ten { v | 0x08 } else { v & !0x08 };
        match i2c::write_byte(io, port, DIAG, offset, v, true) {
            Ok(()) => {}
            Err(Error::I2c { .. }) => ok = false,
            Err(e) => return Err(e),
        }
    }
    Ok(ok)
}
