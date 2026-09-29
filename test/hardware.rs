#[path = "../src/hardware.rs"]
mod hardware;
use hardware::{Error, Family, Link, Registers, Setup};
use std::collections::BTreeMap;

#[derive(Debug, PartialEq, Eq)]
enum Op { Read(u32), Write(u32, u32), Delay(usize) }
struct Fake {
    regs: BTreeMap<u32, u32>,
    ops: Vec<Op>,
    stuck: Option<u32>,
    fail: Option<u32>,
}
impl Fake {
    fn ready(lan: u32) -> Self {
        Self {
            regs: BTreeMap::from([
                (8, lan << 2), (0x10010, 0x300), (0x10110, 1 << (18 + lan)),
                (0x2f00, 8), (0xa200, 0x33221102), (0xa204, 0x80005544),
            ]), ops: vec![], stuck: None, fail: None,
        }
    }
}
impl Registers for Fake {
    type Error = &'static str;
    fn read(&mut self, reg: u32) -> Result<u32, Self::Error> {
        self.ops.push(Op::Read(reg));
        if self.fail == Some(reg) { return Err("PCI read failed"); }
        Ok(*self.regs.get(&reg).unwrap_or(&0))
    }
    fn write(&mut self, reg: u32, value: u32) -> Result<(), Self::Error> {
        self.ops.push(Op::Write(reg, value));
        if self.fail == Some(reg) { return Err("PCI write failed"); }
        if self.stuck != Some(reg) {
            self.regs.insert(reg, if reg == 0 { value & !((1 << 26) | 8) } else { value });
        } else if reg == 0 { self.regs.insert(reg, value); }
        Ok(())
    }
    fn delay_us(&mut self, micros: usize) { self.ops.push(Op::Delay(micros)); }
}
#[test]
fn both_ports_use_nvm_address_and_respect_reset_order() {
    for lan in 0..2 {
        let mut io = Fake::ready(lan);
        io.regs.insert(0x1028, 1 << 25);
        io.regs.insert(0xd028, 1 << 25);
        let id = hardware::reset(&mut io).unwrap();
        assert_eq!(id.mac, [2, 0x11, 0x22, 0x33, 0x44, 0x55]);
        assert_eq!(id.lan, lan as u8);
        let reset = io.ops.iter().position(|op| matches!(op, Op::Write(0, v) if v & (1 << 26) != 0)).unwrap();
        assert_eq!(io.ops[reset + 1], Op::Delay(1000));
        assert_eq!(io.ops[reset + 2], Op::Read(0));
        assert_eq!(io.ops[reset + 3], Op::Delay(10_000));
        assert!(io.ops[..reset].contains(&Op::Write(0x1028, 0)));
        assert!(io.ops[..reset].contains(&Op::Write(0xd028, 0)));
        assert_eq!(io.ops.iter().filter(|op| matches!(op, Op::Write(0x888, _))).count(), 2);
        let disable = io.ops.iter().position(|op| *op == Op::Write(0, 4)).unwrap();
        assert_eq!(&io.ops[disable + 1..disable + 3], &[Op::Read(0), Op::Read(8)]);
    }
}
#[test]
fn no_reset_with_outstanding_pcie_transactions() {
    let mut io = Fake::ready(0);
    io.regs.insert(8, 1 << 19);
    assert!(matches!(hardware::reset(&mut io), Err(Error::Timeout { register: 8, .. })));
    assert!(!io.ops.iter().any(|op| matches!(op, Op::Write(0, v) if v & (1 << 26) != 0)));
    assert_eq!(io.ops.iter().filter(|op| **op == Op::Delay(1000)).count(), 100);
}
#[test]
fn active_virtual_functions_are_not_reset() {
    let mut io = Fake::ready(0);
    io.regs.insert(8, 1 << 18);
    assert_eq!(hardware::reset(&mut io), Err(Error::VirtualizationActive));
    assert!(!io.ops.iter().any(|op| matches!(op, Op::Write(..))));
}
#[test]
fn wedged_queue_fails_before_reset() {
    let mut io = Fake::ready(0);
    io.regs.insert(0xdfe8, 1 << 25); // Queue 127, not only queue zero.
    io.stuck = Some(0xdfe8);
    assert!(matches!(hardware::reset(&mut io), Err(Error::Timeout { register: 0xdfe8, .. })));
    assert!(!io.ops.iter().any(|op| matches!(op, Op::Write(0, _))));
}
#[test]
fn reset_and_nvm_waits_are_bounded() {
    for reg in [0, 0x10010, 0x10110, 0x2f00] {
        let mut io = Fake::ready(0);
        if reg == 0 { io.stuck = Some(0); }
        else { io.regs.insert(reg, 0); }
        assert!(matches!(hardware::reset(&mut io), Err(Error::Timeout { register, .. }) if register == reg));
        assert!(!io.ops.contains(&Op::Read(0xa200)));
        assert!(io.ops.len() < 3000);
    }
}
#[test]
fn absent_nvm_is_not_auto_read_success() {
    let mut io = Fake::ready(0);
    io.regs.insert(0x10010, 1 << 9);
    assert_eq!(hardware::reset(&mut io), Err(Error::MissingNvm));
}
#[test]
fn reject_invalid_macs_but_allow_locally_administered_unicast() {
    for (low, high) in [(0, 0x80000000), (0xffffffff, 0x8000ffff), (3, 0x80000000), (2, 0)] {
        let mut io = Fake::ready(0);
        io.regs.insert(0xa200, low); io.regs.insert(0xa204, high);
        assert_eq!(hardware::reset(&mut io), Err(Error::InvalidMac));
    }
    assert!(hardware::reset(&mut Fake::ready(0)).is_ok());
}
#[test]
fn read_errors_and_removed_device_do_not_become_link_up() {
    let mut io = Fake::ready(0);
    io.fail = Some(hardware::LINKS);
    assert_eq!(hardware::link(&mut io), Err(Error::Io("PCI read failed")));
    io.fail = None; io.regs.insert(hardware::LINKS, u32::MAX);
    assert_eq!(hardware::link(&mut io), Err(Error::Removed));
}
#[test]
fn current_link_and_all_speed_encodings() {
    let mut io = Fake::ready(0);
    for (speed, mbps) in [(0, None), (1, Some(100)), (2, Some(1000)), (3, Some(10000))] {
        io.regs.insert(hardware::LINKS, (1 << 30) | (speed << 28));
        assert_eq!(hardware::link(&mut io), Ok(Link::Up { megabits: mbps }));
    }
    io.regs.insert(hardware::LINKS, (3 << 28) | (1 << 7));
    assert_eq!(hardware::link(&mut io), Ok(Link::Down));
}
#[test]
fn reset_propagates_transport_errors_without_reading_mac() {
    for reg in [0x888, 0x3000, 0x1028, 0, 0x10010, 0x10110] {
        let mut io = Fake::ready(0); io.fail = Some(reg);
        assert!(matches!(hardware::reset(&mut io), Err(Error::Io(_))));
        assert!(!io.ops.contains(&Op::Read(0xa200)));
    }
}
#[test]
fn x82599_applies_nvm_link_mode_with_restart_an_only() {
    let mut io = Fake::ready(0);
    // NVM-loaded: LMS 011 (10G serial), AUTOC2 PMA/PMD SFI, SDP3 driven low.
    let autoc = (3 << 13) | (3 << 30) | (1 << 9);
    io.regs.insert(hardware::AUTOC, autoc);
    io.regs.insert(hardware::AUTOC2, 2 << 16);
    io.regs.insert(hardware::ESDP, 0x0800);
    let setup = hardware::setup_link(&mut io, Family::F82599).unwrap();
    assert_eq!(setup, Setup::Restarted { autoc, autoc2: 2 << 16, esdp: 0x0800 });
    let writes: Vec<_> = io.ops.iter().filter(|op| matches!(op, Op::Write(..))).collect();
    assert_eq!(writes, [&Op::Write(hardware::AUTOC, autoc | (1 << 12))]);
    assert_eq!(hardware::link_mode(autoc, 2 << 16), "10G SFI");
}
#[test]
fn x540_and_x552_link_setup_write_nothing() {
    for (family, expect) in [(Family::X540, Setup::PhyAutonomous), (Family::X552, Setup::Pending)] {
        let mut io = Fake::ready(0);
        assert_eq!(hardware::setup_link(&mut io, family), Ok(expect));
        assert!(io.ops.is_empty());
    }
}
#[test]
fn x82599_link_setup_fails_closed_on_removed_device_or_io_error() {
    let mut io = Fake::ready(0);
    io.regs.insert(hardware::AUTOC, u32::MAX);
    assert_eq!(hardware::setup_link(&mut io, Family::F82599), Err(Error::Removed));
    assert!(!io.ops.iter().any(|op| matches!(op, Op::Write(..))));
    let mut io = Fake::ready(0);
    io.fail = Some(hardware::AUTOC2);
    assert!(matches!(hardware::setup_link(&mut io, Family::F82599), Err(Error::Io(_))));
    assert!(!io.ops.iter().any(|op| matches!(op, Op::Write(..))));
}
#[test]
fn link_mode_names_each_lms() {
    let lms = |v: u32| v << 13;
    assert_eq!(hardware::link_mode(lms(0), 0), "1G SFI");
    assert_eq!(hardware::link_mode(lms(0) | (1 << 9), 0), "1G KX/BX, no AN");
    assert_eq!(hardware::link_mode(lms(1) | (1 << 7), 0), "10G KX4, no AN");
    assert_eq!(hardware::link_mode(lms(1), 0), "10G XAUI, no AN");
    assert_eq!(hardware::link_mode(lms(3), 0), "10G KR, no AN");
    assert_eq!(hardware::link_mode(lms(4), 0), "KX/KX4/KR AN");
    assert_eq!(hardware::link_mode(lms(7), 0), "KX/KX4/KR AN + SGMII");
}
#[test]
fn link_wait_is_bounded_and_returns_early_on_link_up() {
    let mut io = Fake::ready(0);
    assert_eq!(hardware::wait_link(&mut io, 3000), Ok(Link::Down));
    assert_eq!(io.ops.iter().filter(|op| **op == Op::Delay(10_000)).count(), 300);
    let mut io = Fake::ready(0);
    io.regs.insert(hardware::LINKS, (1 << 30) | (3 << 28));
    assert_eq!(hardware::wait_link(&mut io, 3000), Ok(Link::Up { megabits: Some(10_000) }));
    assert!(!io.ops.iter().any(|op| matches!(op, Op::Delay(_))));
    io.regs.insert(hardware::LINKS, u32::MAX);
    assert_eq!(hardware::wait_link(&mut io, 3000), Err(Error::Removed));
}
