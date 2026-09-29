//! 82599 link programming (spec 5, walkthroughs 9.1–9.4).
//!
//! The 82599 MAC has its own KX/KX4/KR/SFI PCS and PMA, configured through
//! AUTOC and AUTOC2; only the T3 LOM (151c) has an external MDIO PHY. The
//! AUTOC/AUTOC2 values right after the MAC reset are the NVM defaults, the
//! "original" snapshot every capability decision reads (spec 5.13).
//!
//! Policy choices the spec leaves to a boot driver: SmartSpeed is not used
//! on backplanes (spec 9.2 allows plain `setup_mac_link`); the Intel-OUI
//! module rule is not applied (spec 4.2 note).

use super::mdio::{self, Speeds, ONE, TEN};
use super::sfp::{self, Kind, Module};
use super::{delay_ms, flush, nvm_word, read, sync, write, Error, Family, Link, Port, Registers,
    AUTOC, AUTOC2, ESDP, LINKS, R};

const ANLP1: u32 = 0x042b0;
const CORECTL: u32 = 0x14f00;
const MANC: u32 = 0x05820;
const FWSM: u32 = 0x10148;
const FACTPS: u32 = 0x10150;

// AUTOC (spec 5.1).
const PMA_1G_KX: u32 = 1 << 9;
const RESTART_AN: u32 = 1 << 12;
const LMS: u32 = 7 << 13;
const LMS_BIT2: u32 = 1 << 15;
const LMS_10G_SERIAL: u32 = 3 << 13;
const KR_SUPP: u32 = 1 << 16;
const KX_SUPP: u32 = 1 << 30;
const KX4_SUPP: u32 = 1 << 31;
// AUTOC2 (spec 5.2).
const PMA_SERIAL: u32 = 3 << 16;
const PMA_SFI: u32 = 2 << 16;
const LINK_DISABLE: u32 = 7 << 28;
// LINKS.
const KX_AN_COMP: u32 = 1 << 31;
// ESDP pins (spec A.2).
const SDP2: u32 = 1 << 2;
const SDP3: u32 = 1 << 3;
const SDP5: u32 = 1 << 5;
const SDP3_DIR: u32 = 1 << 11;
const SDP5_DIR: u32 = 1 << 13;

/// Media by device ID (spec 1.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Media {
    /// KX4, KR, combined backplane, XAUI, CX4: AUTOC only.
    Backplane,
    /// SFP+ cage: module ID, NVM init sequence, laser (SDP3), hard rate select (SDP5).
    Fiber,
    /// Bypass (155d): always multispeed, soft rate select, no laser control.
    FiberFixed,
    /// QSFP+ (1558): shared-bus handshake, no rate select.
    Qsfp,
    /// T3 LOM (151c): external TN1010 10GBASE-T PHY on MDIO.
    Copper,
}

pub fn media(device: u16) -> Media {
    match device {
        0x10fb | 0x1507 | 0x1529 | 0x154a | 0x154d | 0x1557 => Media::Fiber,
        0x155d => Media::FiberFixed,
        0x1558 => Media::Qsfp,
        0x151c => Media::Copper,
        _ => Media::Backplane,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhyReset { Done, Vetoed, OverTemperature }

/// What was found before the MAC reset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prepared {
    Backplane,
    Module(Module),
    Copper { phy: u8, id: u32, reset: PhyReset },
}

/// Before the MAC reset (spec 9.1 step 2, 9.3 steps 1–2, 9.4): identify the
/// module, or find and reset the copper PHY.
pub fn prepare<Io: Registers>(io: &mut Io, port: Port, veto: bool) -> R<Prepared, Io::Error> {
    match media(port.device) {
        Media::Backplane => Ok(Prepared::Backplane),
        Media::Fiber | Media::FiberFixed => Ok(Prepared::Module(sfp::identify(io, port)?)),
        Media::Qsfp => {
            super::i2c::qsfp_setup(io)?;
            Ok(Prepared::Module(sfp::identify_qsfp(io, port)?))
        }
        Media::Copper => {
            let (phy, id) = mdio::scan(io, port)?.ok_or(Error::NoPhy)?;
            // Spec 2.6: not while vetoed, nor while the TN1010 reports
            // over-temperature (1.0x9005 bit 0).
            let reset = if veto {
                PhyReset::Vetoed
            } else if mdio::read_reg(io, port, phy, mdio::PMA, 0x9005)? & 1 != 0 {
                PhyReset::OverTemperature
            } else {
                mdio::reset(io, port, phy, false)?;
                PhyReset::Done
            };
            Ok(Prepared::Copper { phy, id, reset })
        }
    }
}

/// After the MAC reset: clear any AUTOC2 link-disable bits (spec 5.13 step 4).
pub fn after_reset<Io: Registers>(io: &mut Io) -> R<(), Io::Error> {
    let autoc2 = read(io, AUTOC2)?;
    if autoc2 & LINK_DISABLE != 0 { write(io, AUTOC2, autoc2 & !LINK_DISABLE)?; }
    Ok(())
}

/// SFP+ laser state (spec 5.9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Laser {
    /// SDP3 cleared: TX_DISABLE released.
    On,
    /// SDP3 is not an output (ESDP bit 11 clear), so it is not driven.
    NoDirection,
    /// Manageability is enabled: the laser is its business.
    Manageability,
    /// Not SFP+ media (bypass, QSFP): no laser control.
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Setup {
    /// Backplane/CX4: `setup_mac_link` with the NVM capabilities. `written`
    /// is false when AUTOC already matched (the usual case); `an_complete`
    /// is the KX_AN_COMP wait in AN modes after a write.
    Backplane { autoc: u32, autoc2: u32, written: bool, an_complete: Option<bool> },
    /// SFP+/QSFP+. `nvm_autoc` is the snapshot after the MAC reset (the NVM
    /// default every capability decision reads, spec 5.6, 5.13). For a
    /// supported module: `sequence` CORECTL words from the NVM, AUTOC as it
    /// ended up, the laser, the speed the link came up
    /// at while setting it up (multispeed) and the SFI firmware patch version.
    Module {
        module: Module, nvm_autoc: u32, autoc: u32, autoc2: u32, sequence: Option<usize>, laser: Laser,
        speed: Option<u32>, fw: Option<u16>, crosstalk: bool, rate_select: bool,
    },
    /// T3 LOM: TN1010 advertisement and AN restart, then the MAC side restarted.
    Copper { phy: u8, id: u32, reset: PhyReset, advertised: Speeds, restarted: bool },
}

struct Ctx { port: Port, veto: bool, lesm: bool, orig: u32 }

/// Link setup after the MAC reset (spec 9.1–9.4).
pub fn setup<Io: Registers>(io: &mut Io, port: Port, veto: bool, prepared: Prepared) -> R<Setup, Io::Error> {
    let autoc = read(io, AUTOC)?;
    let autoc2 = read(io, AUTOC2)?;
    let ctx = Ctx { port, veto, lesm: lesm(io)?, orig: autoc };
    match prepared {
        Prepared::Backplane => {
            let (speeds, an) = capabilities(autoc, None, false);
            let r = setup_mac_link(io, &ctx, speeds, an, true, false)?;
            Ok(Setup::Backplane { autoc, autoc2, written: r.written, an_complete: r.an_complete })
        }
        Prepared::Module(m) => module_setup(io, &ctx, m, autoc2),
        Prepared::Copper { phy, id, reset } => copper_setup(io, &ctx, phy, id, reset),
    }
}

/// LESM, the NVM's link firmware (spec 5.3): NVM[0x0F] → +2 → +1 bit 15.
fn lesm<Io: Registers>(io: &mut Io) -> R<bool, Io::Error> {
    let Some(fw) = pointer(io, 0x0f)? else { return Ok(false) };
    let Some(p) = pointer(io, fw + 2)? else { return Ok(false) };
    Ok(nvm_word(io, p + 1)? & 0x8000 != 0)
}

/// An NVM pointer word; 0 and 0xFFFF mean none.
fn pointer<Io: Registers>(io: &mut Io, word: u32) -> R<Option<u32>, Io::Error> {
    let v = nvm_word(io, word)?;
    Ok(if v == 0 || v == 0xffff { None } else { Some(v as u32) })
}

/// Manageability enabled (spec 1.5): FWSM pass-through mode, MANC.RCV_TCO_EN,
/// and FACTPS.MNGCG clear.
fn mng_enabled<Io: Registers>(io: &mut Io) -> R<bool, Io::Error> {
    Ok(read(io, FWSM)? & 0xe == 0x4 && read(io, MANC)? & (1 << 17) != 0
        && read(io, FACTPS)? & (1 << 29) == 0)
}

/// Pipeline reset (spec 5.4 step 4): Restart_AN with LMS bit 2 inverted, wait
/// for the AN state to leave 0, then the original LMS with Restart_AN.
fn pipeline_reset<Io: Registers>(io: &mut Io) -> R<(), Io::Error> {
    let autoc2 = read(io, AUTOC2)?;
    if autoc2 & LINK_DISABLE != 0 {
        write(io, AUTOC2, autoc2 & !LINK_DISABLE)?;
        flush(io)?;
    }
    let autoc = read(io, AUTOC)? | RESTART_AN;
    write(io, AUTOC, autoc ^ LMS_BIT2)?;
    let mut left = false;
    for _ in 0..10 {
        delay_ms(io, 4);
        if read(io, ANLP1)? & 0x000f_0000 != 0 { left = true; break; }
    }
    write(io, AUTOC, autoc)?;
    flush(io)?;
    if !left { return Err(Error::PipelineReset); }
    Ok(())
}

/// Protected AUTOC write (spec 5.4): nothing on MNG_VETO; MAC_CSR around it
/// when LESM runs. Returns whether AUTOC was written.
fn protected_write<Io: Registers>(io: &mut Io, ctx: &Ctx, value: u32) -> R<bool, Io::Error> {
    if ctx.veto { return Ok(false); }
    let body = |io: &mut Io| { write(io, AUTOC, value)?; pipeline_reset(io) };
    if ctx.lesm { sync::locked(io, ctx.port, sync::MAC_CSR, body)?; } else { body(io)?; }
    Ok(true)
}

/// Link capabilities (spec 5.6) from the module type, else from the original
/// AUTOC's LMS; multispeed adds 10G and 1G.
fn capabilities(orig: u32, module: Option<(Module, bool)>, qsfp: bool) -> (Speeds, bool) {
    let mut multispeed = false;
    if let Some((m, fixed)) = module {
        multispeed = m.multispeed || fixed;
        if m.one_gig() { return (ONE, true); }
        match m.kind {
            Kind::DaCu => return (Speeds { g10: true, g1: multispeed, m100: false }, true),
            Kind::Bx10g => return (TEN, false),
            _ => {}
        }
    }
    let supp = Speeds { g10: orig & (KR_SUPP | KX4_SUPP) != 0, g1: orig & KX_SUPP != 0, m100: false };
    let (mut s, mut an) = match (orig & LMS) >> 13 {
        0 => (ONE, false),
        1 => (TEN, false),
        2 => (ONE, true),
        3 => (TEN, false),
        4 | 6 => (supp, true),
        7 => (Speeds { m100: true, ..supp }, true),
        _ => (Speeds { g10: false, g1: true, m100: true }, false),
    };
    if multispeed {
        s.g10 = true;
        s.g1 = true;
        an = !qsfp;
    }
    (s, an)
}

struct MacLink { written: bool, an_complete: Option<bool> }

/// `setup_mac_link` (spec 5.7): the AN advertisement from the original
/// AUTOC, or the SFI speed change by LMS; a protected write only if AUTOC
/// changes, then (AN modes, `wait`) KX_AN_COMP up to 4.5 s, and 50 ms.
fn setup_mac_link<Io: Registers>(io: &mut Io, ctx: &Ctx, s: Speeds, autoneg: bool, wait: bool,
    kr_disabled: bool) -> R<MacLink, Io::Error> {
    let cur = read(io, AUTOC)?;
    let autoc2 = read(io, AUTOC2)?;
    let lms = (cur & LMS) >> 13;
    let mut new = cur;
    if matches!(lms, 4 | 6 | 7) {
        new &= !(KX4_SUPP | KX_SUPP | KR_SUPP);
        if s.g10 {
            new |= ctx.orig & KX4_SUPP;
            if !kr_disabled { new |= ctx.orig & KR_SUPP; }
        }
        if s.g1 { new |= KX_SUPP; }
    }
    let sfi = autoc2 & PMA_SERIAL == PMA_SFI;
    let only = |g10: bool| s.g10 == g10 && s.g1 == !g10 && !s.m100;
    if cur & PMA_1G_KX == 0 && (lms == 0 || lms == 2) && only(true) && sfi {
        new = new & !LMS | LMS_10G_SERIAL;
    } else if sfi && lms == 3 && only(false) && cur & PMA_1G_KX == 0 {
        new = new & !LMS | if autoneg { 2 << 13 } else { 0 };
    }
    if new == cur { return Ok(MacLink { written: false, an_complete: None }); }
    let written = protected_write(io, ctx, new)?;
    let mut an_complete = None;
    if wait && matches!((new & LMS) >> 13, 4 | 6 | 7) {
        let mut done = false;
        for _ in 0..45 {
            if read(io, LINKS)? & KX_AN_COMP != 0 { done = true; break; }
            delay_ms(io, 100);
        }
        an_complete = Some(done);
    }
    delay_ms(io, 50);
    Ok(MacLink { written, an_complete })
}

/// The NVM init sequence for `key` (spec 5.5 steps 2–4): the list at
/// NVM[0x2B] is (ID, data pointer) pairs ended by 0xFFFF; the data block's
/// words after the first go to CORECTL, under MAC_CSR, until 0xFFFF.
fn init_sequence<Io: Registers>(io: &mut Io, port: Port, key: u16) -> R<usize, Io::Error> {
    let missing = || Error::NoInitSequence { key };
    let Some(list) = pointer(io, 0x2b)? else { return Err(missing()) };
    let mut word = list + 1;
    let data = loop {
        if word >= 0x3fff { return Err(missing()); }
        let id = nvm_word(io, word)?;
        if id == 0xffff { return Err(missing()); }
        if id == key {
            match pointer(io, word + 1)? { Some(p) => break p, None => return Err(missing()) }
        }
        word += 2;
    };
    sync::locked(io, port, sync::MAC_CSR, |io| {
        let mut n = 0;
        let mut word = data + 1;
        while word < 0x3fff {
            let v = nvm_word(io, word)?;
            if v == 0xffff { break; }
            write(io, CORECTL, v as u32)?;
            flush(io)?;
            n += 1;
            word += 1;
        }
        Ok(n)
    })
}

/// SFI firmware patch version (spec 5.12), for the console only.
fn sfi_fw_version<Io: Registers>(io: &mut Io) -> R<Option<u16>, Io::Error> {
    let Some(fw) = pointer(io, 0x0f)? else { return Ok(None) };
    let Some(ptp) = pointer(io, fw + 4)? else { return Ok(None) };
    Ok(Some(nvm_word(io, ptp + 7)?))
}

fn laser<Io: Registers>(io: &mut Io, on: bool) -> R<(), Io::Error> {
    let esdp = read(io, ESDP)?;
    write(io, ESDP, if on { esdp & !SDP3 } else { esdp | SDP3 })?;
    flush(io)?;
    io.delay_us(if on { 100_000 } else { 100 });
    Ok(())
}

/// Hard rate select on SDP5 (spec 4.3), soft rate select for the bypass
/// part; QSFP has none. Returns false if a soft select failed.
fn rate_select<Io: Registers>(io: &mut Io, port: Port, media: Media, ten: bool) -> R<bool, Io::Error> {
    match media {
        Media::Fiber => {
            let esdp = read(io, ESDP)? | SDP5_DIR;
            write(io, ESDP, if ten { esdp | SDP5 } else { esdp & !SDP5 })?;
            flush(io)?;
            Ok(true)
        }
        Media::FiberFixed => sfp::soft_rate_select(io, port, ten),
        _ => Ok(true),
    }
}

/// LINKS for the 82599 (spec 4.4, 8.1): with the crosstalk fix active an
/// empty cage (SDP2 clear) is link down, and "up" is read again after 5 ms.
pub fn link<Io: Registers>(io: &mut Io, crosstalk: bool) -> R<Link, Io::Error> {
    if crosstalk && read(io, ESDP)? & SDP2 == 0 { return Ok(Link::Down); }
    let state = super::link(io, Family::F82599)?;
    if crosstalk && state != Link::Down {
        delay_ms(io, 5);
        return super::link(io, Family::F82599);
    }
    Ok(state)
}

fn up<Io: Registers>(io: &mut Io, crosstalk: bool) -> R<bool, Io::Error> {
    Ok(link(io, crosstalk)? != Link::Down)
}

struct Speedy { media: Media, an: bool, crosstalk: bool, flap: bool, rate_ok: bool }

/// One speed of the multispeed algorithm (spec 5.8): rate select, 40 ms,
/// `setup_mac_link`, the laser flap if pending, then the link check (10 ×
/// 100 ms at 10G, one check after 100 ms at 1G).
fn try_speed<Io: Registers>(io: &mut Io, ctx: &Ctx, st: &mut Speedy, ten: bool) -> R<bool, Io::Error> {
    st.rate_ok &= rate_select(io, ctx.port, st.media, ten)?;
    delay_ms(io, 40);
    setup_mac_link(io, ctx, if ten { TEN } else { ONE }, st.an, false, false)?;
    if st.flap {
        if !ctx.veto { laser(io, false)?; }
        laser(io, true)?;
        st.flap = false;
    }
    if ten {
        for _ in 0..10 {
            delay_ms(io, 100);
            if up(io, st.crosstalk)? { return Ok(true); }
        }
        return Ok(false);
    }
    delay_ms(io, 100);
    up(io, st.crosstalk)
}

/// Multispeed fiber (spec 5.8): 10G, then 1G; with no link after trying
/// both, 10G again, where the port is left waiting.
fn multispeed<Io: Registers>(io: &mut Io, ctx: &Ctx, st: &mut Speedy, s: Speeds) -> R<Option<u32>, Io::Error> {
    if s.g10 && try_speed(io, ctx, st, true)? { return Ok(Some(10_000)); }
    if s.g1 && try_speed(io, ctx, st, false)? { return Ok(Some(1000)); }
    if s.g10 && s.g1 && try_speed(io, ctx, st, true)? { return Ok(Some(10_000)); }
    Ok(None)
}

/// SFP+/QSFP+ (spec 9.1, 9.4) after the MAC reset.
fn module_setup<Io: Registers>(io: &mut Io, ctx: &Ctx, m: Module, autoc2: u32) -> R<Setup, Io::Error> {
    let media = media(ctx.port.device);
    let mut setup = Setup::Module {
        module: m, nvm_autoc: ctx.orig, autoc: ctx.orig, autoc2, sequence: None, laser: Laser::None, speed: None,
        fw: None, crosstalk: false, rate_select: true,
    };
    if !m.supported(Family::F82599) { return Ok(setup); }
    let fw = sfi_fw_version(io)?;
    // Spec 5.5: NVM init sequence into CORECTL, 10 ms, then SFI (LMS 011
    // OR'd into the original AUTOC, spec 10 item 10).
    let key = m.nvm_key(ctx.port.lan).unwrap_or(0xffff);
    let words = init_sequence(io, ctx.port, key)?;
    delay_ms(io, 10);
    protected_write(io, ctx, ctx.orig | LMS_10G_SERIAL)?;
    // Spec 5.9: the laser, unless manageability runs it or SDP3 is no output.
    let laser_state = if media != Media::Fiber {
        Laser::None
    } else if mng_enabled(io)? {
        Laser::Manageability
    } else if read(io, ESDP)? & SDP3_DIR == 0 {
        Laser::NoDirection
    } else {
        laser(io, true)?;
        Laser::On
    };
    // Spec 4.4: NVM word 0x2C bit 7 clear on SFP+ media: the crosstalk fix.
    let crosstalk = media == Media::Fiber && nvm_word(io, 0x2c)? & 0x80 == 0;
    let fixed = media == Media::FiberFixed;
    let (s, an) = capabilities(ctx.orig, Some((m, fixed)), media == Media::Qsfp);
    let mut st = Speedy { media, an, crosstalk, flap: laser_state == Laser::On, rate_ok: true };
    let speed = if (m.multispeed || fixed) && media != Media::Qsfp {
        multispeed(io, ctx, &mut st, s)?
    } else {
        setup_mac_link(io, ctx, s, an, false, false)?;
        None
    };
    if let Setup::Module { autoc, sequence, laser, speed: sp, fw: f, crosstalk: c, rate_select, .. } = &mut setup {
        *autoc = read(io, AUTOC)?;
        *sequence = Some(words);
        *laser = laser_state;
        *sp = speed;
        *f = fw;
        *c = crosstalk;
        *rate_select = st.rate_ok;
    }
    Ok(setup)
}

/// T3 LOM (spec 5.11, 9.3): advertise what the TN1010 can do (1G in the XNP
/// transmit register 7.0x0017 bit 14), restart AN unless vetoed, then a
/// pipeline reset of the MAC side and 50 ms.
fn copper_setup<Io: Registers>(io: &mut Io, ctx: &Ctx, phy: u8, id: u32, reset: PhyReset) -> R<Setup, Io::Error> {
    let port = ctx.port;
    let s = mdio::abilities(io, port, phy)?;
    let set = |v: u16, mask: u16, on: bool| if on { v | mask } else { v & !mask };
    mdio::modify(io, port, phy, mdio::AN, 0x0020, |v| set(v, 1 << 12, s.g10))?;
    mdio::modify(io, port, phy, mdio::AN, 0x0017, |v| set(v, 1 << 14, s.g1))?;
    mdio::modify(io, port, phy, mdio::AN, 0x0010, |v| set(v, 1 << 8, s.m100))?;
    let restarted = !ctx.veto;
    if restarted {
        mdio::modify(io, port, phy, mdio::AN, 0x0000, |v| v | (1 << 9))?;
        if ctx.lesm { sync::locked(io, port, sync::MAC_CSR, pipeline_reset)?; } else { pipeline_reset(io)?; }
    }
    delay_ms(io, 50);
    Ok(Setup::Copper { phy, id, reset, advertised: s, restarted })
}
