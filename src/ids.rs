//! The PCI functions this driver binds: Intel (8086) 10 GbE physical
//! functions of the 82599, X540 and X552 families.
//!
//! Device IDs from the Intel datasheets (82599 10 GbE Controller Datasheet,
//! X540 Datasheet, Xeon D-1500 / X552 datasheet) and, for the board variants
//! the datasheets leave to the adapter, the PCI ID registry.
//! Virtual functions (82599 10ed, X540 1515, X552 15a8) are not listed: a
//! VF has no PHY of its own and is not what firmware boots from.

pub const INTEL: u16 = 0x8086;

/// PCI base class of a network controller (config offset 0x0b).
pub const CLASS_NETWORK: u8 = 0x02;

pub struct Nic {
    pub device: u16,
    pub name: &'static str,
}

pub const SUPPORTED: &[Nic] = &[
    // 82599
    Nic { device: 0x10f7, name: "82599 KX4" },
    Nic { device: 0x10f8, name: "82599 combined backplane" },
    Nic { device: 0x10f9, name: "82599 CX4" },
    Nic { device: 0x10fb, name: "82599 SFP+" },
    Nic { device: 0x10fc, name: "82599 XAUI" },
    Nic { device: 0x1507, name: "82599 express module" },
    Nic { device: 0x1514, name: "82599 KX4/KR mezzanine" },
    Nic { device: 0x1517, name: "82599 KR" },
    Nic { device: 0x151c, name: "82599 10GBASE-T" },
    Nic { device: 0x1529, name: "82599 SFP+ FCoE" },
    Nic { device: 0x152a, name: "82599 backplane FCoE" },
    Nic { device: 0x154a, name: "82599 SFP+ quad" },
    Nic { device: 0x154d, name: "82599 SFP+ SF2" },
    Nic { device: 0x1557, name: "82599EN SFP+" },
    Nic { device: 0x1558, name: "82599 QSFP+" },
    // X540
    Nic { device: 0x1528, name: "X540-T" },
    Nic { device: 0x1560, name: "X540-T1" },
    // X552 (Xeon D-1500)
    Nic { device: 0x15aa, name: "X552 backplane" },
    Nic { device: 0x15ab, name: "X552 backplane" },
    Nic { device: 0x15ac, name: "X552 SFP+" },
    Nic { device: 0x15ad, name: "X552/X557-AT 10GBASE-T" },
    Nic { device: 0x15ae, name: "X552 1000BASE-T" },
];

pub fn lookup(vendor: u16, device: u16) -> Option<&'static Nic> {
    if vendor != INTEL {
        return None;
    }
    SUPPORTED.iter().find(|n| n.device == device)
}
