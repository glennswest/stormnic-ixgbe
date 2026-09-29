//! The PCI functions this driver binds: Intel (8086) 10 GbE physical
//! functions of the 82599, X540 and X552 families.
//!
//! Device IDs from the Intel datasheets (82599 10 GbE Controller Datasheet,
//! X540 Datasheet, Xeon D-1500 / X552 datasheet), the PCI ID registry for
//! board variants, and docs/spec/phy.md 1.2, which lists 26 physical
//! functions. 25 are here. 82599_LS (154f) is left out: the shared code has
//! no MAC type for it and its identification path is unclear (spec 10
//! item 2). Virtual functions (82599 10ed, 152e; X540 1515, 1530; X552
//! 15a8, 15a9) are not listed: a VF has no PHY access and must not be bound.

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
    Nic { device: 0x1557, name: "82599EN SFP+", family: Family::F82599 },
    Nic { device: 0x1558, name: "82599 QSFP+", family: Family::F82599 },
    Nic { device: 0x155d, name: "82599 bypass", family: Family::F82599 },
    // X540
    Nic { device: 0x1528, name: "X540-T", family: Family::X540 },
    Nic { device: 0x1560, name: "X540-T1", family: Family::X540 },
    Nic { device: 0x155c, name: "X540 bypass", family: Family::X540 },
    // X552 (Xeon D-1500)
    Nic { device: 0x15aa, name: "X552 KX4", family: Family::X552 },
    Nic { device: 0x15ab, name: "X552 KR", family: Family::X552 },
    Nic { device: 0x15ac, name: "X552 SFP+", family: Family::X552 },
    Nic { device: 0x15ad, name: "X552/X557-AT 10GBASE-T", family: Family::X552 },
    Nic { device: 0x15ae, name: "X552 1000BASE-T", family: Family::X552 },
    Nic { device: 0x15b0, name: "X552 XFI", family: Family::X552 },
];

pub fn lookup(vendor: u16, device: u16) -> Option<&'static Nic> {
    if vendor != INTEL {
        return None;
    }
    SUPPORTED.iter().find(|n| n.device == device)
}
