//! The PCI functions this driver binds: Intel (8086) 10 GbE physical
//! functions of the 82599, X540, X550, X552 and X553 families.
//!
//! Device IDs from the Intel datasheets (82599 10 GbE Controller Datasheet,
//! X540 Datasheet, Xeon D-1500 / X552 datasheet), the PCI ID registry for
//! board variants, and docs/spec/phy.md 1.2, which lists 26 physical
//! functions; all 26 are here. 82599_LS (154f) is missing from the shared
//! code's MAC-type table, but its media type (`fiber_lco`) gives it the
//! backplane path, and Linux binds it as an 82599 (spec 11.1, #16).
//! The X550 (1563, 15d1) and the X553 (nine IDs) are from spec 12.1 (#23).
//! The X553 QSFP+ IDs (15ca, 15cc) are not bound: the shared code has no
//! module path for them (spec 12.1).
//! Virtual functions (82599 10ed, 152e; X540 1515, 1530; X550 1564, 1565;
//! X552 15a8, 15a9; X553 15b4, 15c5) are not listed: a VF has no PHY access
//! and must not be bound.

pub const INTEL: u16 = 0x8086;

/// PCI base class of a network controller (config offset 0x0b).
pub const CLASS_NETWORK: u8 = 0x02;

pub use crate::hardware::Family;

pub struct Nic {
    pub device: u16,
    pub name: &'static str,
    pub family: Family,
}

pub const SUPPORTED: &[Nic] = &[
    // 82599
    Nic { device: 0x10f7, name: "82599 KX4", family: Family::F82599 },
    Nic { device: 0x10f8, name: "82599 combined backplane", family: Family::F82599 },
    Nic { device: 0x10f9, name: "82599 CX4", family: Family::F82599 },
    Nic { device: 0x10fb, name: "82599 SFP+", family: Family::F82599 },
    Nic { device: 0x10fc, name: "82599 XAUI", family: Family::F82599 },
    Nic { device: 0x1507, name: "82599 express module", family: Family::F82599 },
    Nic { device: 0x1514, name: "82599 KX4/KR mezzanine", family: Family::F82599 },
    Nic { device: 0x1517, name: "82599 KR", family: Family::F82599 },
    Nic { device: 0x151c, name: "82599 10GBASE-T", family: Family::F82599 },
    Nic { device: 0x1529, name: "82599 SFP+ FCoE", family: Family::F82599 },
    Nic { device: 0x152a, name: "82599 backplane FCoE", family: Family::F82599 },
    Nic { device: 0x154a, name: "82599 SFP+ quad", family: Family::F82599 },
    Nic { device: 0x154d, name: "82599 SFP+ SF2", family: Family::F82599 },
    Nic { device: 0x154f, name: "82599 LS", family: Family::F82599 },
    Nic { device: 0x1557, name: "82599EN SFP+", family: Family::F82599 },
    Nic { device: 0x1558, name: "82599 QSFP+", family: Family::F82599 },
    Nic { device: 0x155d, name: "82599 bypass", family: Family::F82599 },
    // X540
    Nic { device: 0x1528, name: "X540-T", family: Family::X540 },
    Nic { device: 0x1560, name: "X540-T1", family: Family::X540 },
    Nic { device: 0x155c, name: "X540 bypass", family: Family::X540 },
    // X550
    Nic { device: 0x1563, name: "X550-T2", family: Family::X550 },
    Nic { device: 0x15d1, name: "X550-T1", family: Family::X550 },
    // X552 (Xeon D-1500)
    Nic { device: 0x15aa, name: "X552 KX4", family: Family::X552 },
    Nic { device: 0x15ab, name: "X552 KR", family: Family::X552 },
    Nic { device: 0x15ac, name: "X552 SFP+", family: Family::X552 },
    Nic { device: 0x15ad, name: "X552/X557-AT 10GBASE-T", family: Family::X552 },
    Nic { device: 0x15ae, name: "X552 1000BASE-T", family: Family::X552 },
    Nic { device: 0x15b0, name: "X552 XFI", family: Family::X552 },
    // X553 (Atom C3000)
    Nic { device: 0x15c2, name: "X553 KR", family: Family::X553 },
    Nic { device: 0x15c3, name: "X553 L KR", family: Family::X553 },
    Nic { device: 0x15c4, name: "X553 N SFP+", family: Family::X553 },
    Nic { device: 0x15c6, name: "X553 SGMII", family: Family::X553 },
    Nic { device: 0x15c7, name: "X553 L SGMII", family: Family::X553 },
    Nic { device: 0x15c8, name: "X553/X557-AT 10GBASE-T", family: Family::X553 },
    Nic { device: 0x15ce, name: "X553 SFP+", family: Family::X553 },
    Nic { device: 0x15e4, name: "X553 1GbE", family: Family::X553 },
    Nic { device: 0x15e5, name: "X553 L 1GbE", family: Family::X553 },
];

pub fn lookup(vendor: u16, device: u16) -> Option<&'static Nic> {
    if vendor != INTEL {
        return None;
    }
    SUPPORTED.iter().find(|n| n.device == device)
}
