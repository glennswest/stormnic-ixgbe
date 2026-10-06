//! X540 and X550 integrated 10GBASE-T PHY (spec 6, walkthrough 9.5, 12.3).
//!
//! The PHY runs its own firmware and is never soft-reset by software. After
//! the MAC reset (CTRL.RST under the PHY semaphore, 100 ms, in `reset`),
//! software finds the PHY, powers it on, advertises what it can do and
//! restarts AN. Link and speed come from LINKS. The X550 also advertises
//! 2.5G and 5G.

use super::mdio::{self, Speeds};
use super::{Error, Family, Port, Registers, R};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Setup {
    pub phy: u8,
    pub id: u32,
    pub advertised: Speeds,
    /// X550: 5G and 2.5G advertised as well (spec 12.3).
    pub nbase: bool,
    /// False when MMNGC.MNG_VETO kept AN from being restarted.
    pub restarted: bool,
}

pub fn setup<Io: Registers>(io: &mut Io, port: Port, veto: bool) -> R<Setup, Io::Error> {
    // Spec 6.2: scan 0–31, expect 0x0154_0200; the first responder wins.
    let (phy, id) = mdio::scan(io, port)?.ok_or(Error::NoPhy)?;
    // Spec 6.3: out of low-power mode (30.0x0000 bit 11), unconditionally: an
    // earlier operating system may have left it there.
    mdio::modify(io, port, phy, mdio::VENDOR, 0x0000, |v| v & !0x0800)?;
    // Spec 6.4, 6.5: advertise every speed 1.0x0004 reports, then restart;
    // on the X550, 2.5G and 5G always (spec 12.3).
    let advertised = mdio::abilities(io, port, phy)?;
    let nbase = port.family == Family::X550;
    let restarted = mdio::advertise(io, port, phy, advertised, nbase.then_some(true), veto)?;
    Ok(Setup { phy, id, advertised, nbase, restarted })
}
