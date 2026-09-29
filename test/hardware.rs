#[path = "../src/hardware.rs"]
mod hardware;
use hardware::x552::{self, Copper, Internal, Module};
use hardware::{Error, Family, Link, Registers, Setup};
use std::collections::BTreeMap;

#[derive(Debug, PartialEq, Eq)]
enum Op { Read(u32), Write(u32, u32), Delay(usize) }
struct Fake {
    regs: BTreeMap<u32, u32>,
    ops: Vec<Op>,
    stuck: Option<u32>,
    fail: Option<u32>,
    /// X552 KR PHY registers behind the IOSF sideband, and the writes to them.
    kr: BTreeMap<u32, u32>,
    kr_writes: Vec<(u32, u32)>,
    iosf_addr: u32,
    iosf_error: bool,
    /// Clause 45 registers by (PHY, device, register); absent reads 0xffff.
    mdio: BTreeMap<(u32, u32, u32), u16>,
    mdio_addr: (u32, u32, u32),
    mdio_writes: Vec<((u32, u32, u32), u16)>,
    i2c: Slave,
}
impl Fake {
    fn ready(lan: u32) -> Self {
        Self {
            regs: BTreeMap::from([
                (8, lan << 2), (0x10010, 0x300), (0x10110, 1 << (18 + lan)),
                (0x2f00, 8), (0xa200, 0x33221102), (0xa204, 0x80005544),
            ]), ops: vec![], stuck: None, fail: None,
            kr: BTreeMap::new(), kr_writes: vec![], iosf_addr: 0, iosf_error: false,
            mdio: BTreeMap::new(), mdio_addr: (0, 0, 0), mdio_writes: vec![],
            i2c: Slave::default(),
        }
    }
    fn writes(&self) -> usize { self.ops.iter().filter(|op| matches!(op, Op::Write(..))).count() }
    fn delays(&self, micros: usize) -> usize { self.ops.iter().filter(|op| **op == Op::Delay(micros)).count() }
}
const IOSF_CTRL: u32 = 0x11144;
const IOSF_DATA: u32 = 0x11148;
const MSCA: u32 = 0x425c;
const MSRWD: u32 = 0x4260;
const I2CCTL: u32 = 0x15f5c;
const SWSM: u32 = 0x10140;
const SWFW_SYNC: u32 = 0x10160;
const ESDP: u32 = 0x20;
impl Registers for Fake {
    type Error = &'static str;
    fn read(&mut self, reg: u32) -> Result<u32, Self::Error> {
        self.ops.push(Op::Read(reg));
        if self.fail == Some(reg) { return Err("PCI read failed"); }
        let value = *self.regs.get(&reg).unwrap_or(&0);
        if reg == I2CCTL {
            let lines = if self.i2c.scl { 1 << 14 } else { 0 } | if self.i2c.sda { 1 << 12 } else { 0 };
            return Ok(value & !((1 << 14) | (1 << 12)) | lines);
        }
        Ok(value)
    }
    fn write(&mut self, reg: u32, value: u32) -> Result<(), Self::Error> {
        self.ops.push(Op::Write(reg, value));
        if self.fail == Some(reg) { return Err("PCI write failed"); }
        if self.stuck != Some(reg) {
            self.regs.insert(reg, if reg == 0 { value & !((1 << 26) | 8) } else { value });
        } else if reg == 0 { self.regs.insert(reg, value); }
        match reg {
            IOSF_CTRL => {
                self.iosf_addr = value & 0xffff;
                let status = if self.iosf_error { 1 << 18 } else { 0 };
                self.regs.insert(IOSF_CTRL, (value & !(1 << 31)) | status);
                self.regs.insert(IOSF_DATA, *self.kr.get(&self.iosf_addr).unwrap_or(&0));
            }
            IOSF_DATA => {
                self.kr.insert(self.iosf_addr, value);
                self.kr_writes.push((self.iosf_addr, value));
            }
            MSCA => {
                let key = ((value >> 21) & 0x1f, (value >> 16) & 0x1f, value & 0xffff);
                match (value >> 26) & 3 {
                    0 => self.mdio_addr = key,
                    3 => {
                        let data = *self.mdio.get(&self.mdio_addr).unwrap_or(&0xffff) as u32;
                        self.regs.insert(MSRWD, data << 16);
                    }
                    1 => {
                        let data = (self.regs[&MSRWD] & 0xffff) as u16;
                        self.mdio.insert(self.mdio_addr, data);
                        self.mdio_writes.push((self.mdio_addr, data));
                    }
                    _ => panic!("unexpected MDIO opcode"),
                }
                self.regs.insert(MSCA, value & !(1 << 30));
            }
            I2CCTL => self.i2c.drive(value),
            _ => {}
        }
        Ok(())
    }
    fn delay_us(&mut self, micros: usize) { self.ops.push(Op::Delay(micros)); }
}

/// An I2C bus with the X552 SFP+ board's slaves, driven edge by edge from
/// I2CCTL writes: SFP ID EEPROM (0xa0), port expander (0xe0), and the
/// CS4227's checksummed 16-bit "combined" register protocol (0xbe).
#[derive(Default, PartialEq, Debug)]
enum Phase { #[default] Idle, Rx, RxAck, Tx, TxAck }
#[derive(Default)]
struct Slave {
    scl: bool, sda: bool, pull: bool,
    phase: Phase, shift: u8, n: u8, expect_addr: bool, reading: bool, acked: bool,
    txn: Vec<u8>, tx: Vec<u8>, cur: u8,
    sfp: Option<Vec<u8>>,
    pe: [u8; 4],
    cs: BTreeMap<u16, u16>,
    cs_resets: usize,
}
fn ones_sum(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0u8, |acc, &b| { let s = acc as u16 + b as u16; ((s & 0xff) + (s >> 8)) as u8 })
}
impl Slave {
    fn cs4227(regs: &[(u16, u16)]) -> Self {
        // Port expander bit 1: CS4227 reset line, an input (released) at power-on.
        let mut s = Slave { cs: regs.iter().copied().collect(), pe: [0, 2, 0, 2], ..Default::default() };
        s.cs.entry(0x181).or_insert(1);
        s.cs.entry(0x5001).or_insert(1);
        s
    }
    fn present(&self, addr: u8) -> bool {
        match addr & 0xfe { 0xa0 => self.sfp.is_some(), 0xe0 | 0xbe => true, _ => false }
    }
    fn drive(&mut self, ctl: u32) {
        let scl = ctl & ((1 << 9) | (1 << 13)) != 0;
        let master = ctl & ((1 << 10) | (1 << 11)) != 0;
        let sda = master && !self.pull;
        if self.scl && scl && sda != self.sda {
            if sda { self.stop() } else { self.start() }
        } else if !self.scl && scl {
            self.rise(sda);
        } else if self.scl && !scl {
            self.fall();
        }
        self.scl = scl;
        self.sda = master && !self.pull;
    }
    fn start(&mut self) {
        self.phase = Phase::Rx; self.n = 0; self.shift = 0; self.expect_addr = true; self.pull = false;
    }
    fn stop(&mut self) {
        if !self.txn.is_empty() && !self.reading { self.apply_write(); }
        self.txn.clear(); self.phase = Phase::Idle; self.pull = false; self.reading = false;
    }
    fn rise(&mut self, sda: bool) {
        match self.phase {
            Phase::Rx => { self.shift = self.shift << 1 | sda as u8; self.n += 1; }
            Phase::TxAck => self.acked = !sda,
            _ => {}
        }
    }
    fn next_tx(&mut self) -> u8 { if self.tx.is_empty() { 0xff } else { self.tx.remove(0) } }
    fn fall(&mut self) {
        match self.phase {
            Phase::Rx if self.n == 8 => {
                let byte = self.shift;
                if self.expect_addr {
                    self.expect_addr = false;
                    if !self.present(byte) { self.phase = Phase::Idle; return; }
                    self.reading = byte & 1 != 0;
                    self.txn.push(byte);
                    if self.reading { self.tx = self.respond(); }
                } else {
                    self.txn.push(byte);
                }
                self.pull = true;
                self.phase = Phase::RxAck;
            }
            Phase::RxAck => {
                self.pull = false;
                if self.reading {
                    self.cur = self.next_tx(); self.n = 0; self.phase = Phase::Tx;
                    self.pull = self.cur & 0x80 == 0;
                } else {
                    self.phase = Phase::Rx; self.n = 0; self.shift = 0;
                }
            }
            Phase::Tx => {
                self.n += 1;
                if self.n == 8 { self.pull = false; self.phase = Phase::TxAck; }
                else { self.pull = (self.cur >> (7 - self.n)) & 1 == 0; }
            }
            Phase::TxAck => {
                if self.acked {
                    self.cur = self.next_tx(); self.n = 0; self.phase = Phase::Tx;
                    self.pull = self.cur & 0x80 == 0;
                } else { self.phase = Phase::Idle; self.pull = false; }
            }
            _ => {}
        }
    }
    fn respond(&mut self) -> Vec<u8> {
        let t = &self.txn;
        match t[0] {
            0xa0 => self.sfp.as_ref().unwrap()[t[1] as usize..].to_vec(),
            0xe0 => vec![self.pe[t[1] as usize]],
            0xbe => {
                assert_eq!(t.len(), 5, "combined read: dev, reg high, reg low, csum, dev|1");
                assert_eq!(t[1] & 1, 1, "combined read flag");
                assert_eq!(ones_sum(&t[1..4]), 0xff, "combined read checksum");
                let reg = ((t[1] & 0xfe) as u16) << 7 | t[2] as u16;
                let v = *self.cs.get(&reg).unwrap_or(&0);
                let (hi, lo) = ((v >> 8) as u8, v as u8);
                vec![hi, lo, !ones_sum(&[hi, lo])]
            }
            _ => unreachable!(),
        }
    }
    fn apply_write(&mut self) {
        let t = self.txn.clone();
        match t[0] {
            0xe0 => {
                assert_eq!(t.len(), 3);
                let (reg, value) = (t[1] as usize, t[2]);
                // Output bit 1 rising while configured as an output: CS4227 out of reset.
                if reg == 1 && self.pe[3] & 2 == 0 && self.pe[1] & 2 == 0 && value & 2 != 0 {
                    self.cs_resets += 1;
                }
                self.pe[reg] = value;
            }
            0xbe => {
                assert_eq!(t.len(), 6, "combined write: dev, reg high, reg low, data high, low, csum");
                assert_eq!(t[1] & 1, 0, "combined write flag");
                assert_eq!(ones_sum(&t[1..6]), 0xff, "combined write checksum");
                let reg = ((t[1] & 0xfe) as u16) << 7 | t[2] as u16;
                self.cs.insert(reg, (t[3] as u16) << 8 | t[4] as u16);
            }
            _ => panic!("unexpected I2C write {t:02x?}"),
        }
    }
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
    let setup = hardware::setup_link(&mut io, Family::F82599, 0x1557, 0).unwrap();
    assert_eq!(setup, Setup::Restarted { autoc, autoc2: 2 << 16, esdp: 0x0800 });
    let writes: Vec<_> = io.ops.iter().filter(|op| matches!(op, Op::Write(..))).collect();
    assert_eq!(writes, [&Op::Write(hardware::AUTOC, autoc | (1 << 12))]);
    assert_eq!(hardware::link_mode(autoc, 2 << 16), "10G SFI");
}
#[test]
fn x540_x552_kx4_and_1g_t_link_setup_write_nothing() {
    for (family, device, expect) in [
        (Family::X540, 0x1528, Setup::PhyAutonomous),
        (Family::X552, 0x15aa, Setup::X552(x552::Setup::Kx4)),
        (Family::X552, 0x15ae, Setup::X552(x552::Setup::FirmwarePhy)),
    ] {
        let mut io = Fake::ready(0);
        assert_eq!(hardware::setup_link(&mut io, family, device, 0), Ok(expect));
        assert!(io.ops.is_empty());
    }
}
#[test]
fn x82599_link_setup_fails_closed_on_removed_device_or_io_error() {
    let mut io = Fake::ready(0);
    io.regs.insert(hardware::AUTOC, u32::MAX);
    assert_eq!(hardware::setup_link(&mut io, Family::F82599, 0x1557, 0), Err(Error::Removed));
    assert!(!io.ops.iter().any(|op| matches!(op, Op::Write(..))));
    let mut io = Fake::ready(0);
    io.fail = Some(hardware::AUTOC2);
    assert!(matches!(hardware::setup_link(&mut io, Family::F82599, 0x1557, 0), Err(Error::Io(_))));
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

const LC1_AN_ENABLE: u32 = 1 << 29;
const LC1_AN_RESTART: u32 = 1 << 31;
const LC1_KR: u32 = 1 << 18;
const LC1_KX: u32 = 1 << 16;

fn released(io: &Fake) {
    assert_eq!(io.regs.get(&SWFW_SYNC).copied().unwrap_or(0) & 0x1fff, 0, "SW_FW_SYNC software bits released");
    assert_eq!(io.regs.get(&SWSM).copied().unwrap_or(0) & 1, 0, "SWSM.SMBI released");
}
#[test]
fn x552_kr_advertises_kr_and_kx_and_restarts_an_on_its_own_port() {
    for (lan, reg, other) in [(0u8, 0x420c, 0x820c), (1, 0x820c, 0x420c)] {
        let mut io = Fake::ready(lan as u32);
        io.kr.insert(reg, (1 << 24) | (4 << 8)); // unrelated bits kept, stale forced speed too
        let setup = hardware::setup_link(&mut io, Family::X552, 0x15ab, lan).unwrap();
        let lc1 = (1 << 24) | (4 << 8) | LC1_AN_ENABLE | LC1_KR | LC1_KX;
        assert_eq!(setup, Setup::X552(x552::Setup::Kr { link_ctrl: lc1 }));
        assert_eq!(io.kr_writes, [(reg, lc1), (reg, lc1 | LC1_AN_RESTART)]);
        assert!(!io.kr.contains_key(&other));
        // Sideband target KR PHY (0) and the register address in IOSF_CTRL.
        assert!(io.ops.contains(&Op::Write(IOSF_CTRL, reg)));
        released(&io);
    }
}
#[test]
fn x552_kr_leaves_link_to_manageability_on_veto() {
    let mut io = Fake::ready(0);
    io.regs.insert(0x42d0, 1);
    assert_eq!(hardware::setup_link(&mut io, Family::X552, 0x15ab, 0),
        Ok(Setup::X552(x552::Setup::ManageabilityVeto)));
    assert_eq!(io.writes(), 0);
}
#[test]
fn x552_semaphore_held_by_firmware_fails_closed_and_bounded() {
    let mut io = Fake::ready(0);
    io.regs.insert(SWFW_SYNC, 0x2 << 5); // firmware holds PHY0
    assert_eq!(hardware::setup_link(&mut io, Family::X552, 0x15ab, 0), Err(Error::Semaphore { held: 0x40 }));
    assert!(io.kr_writes.is_empty());
    assert!(!io.ops.iter().any(|op| matches!(op, Op::Write(IOSF_CTRL, _))));
    assert_eq!(io.regs[&SWFW_SYNC], 0x40, "firmware's bit left alone, no software bit taken");
    assert_eq!(io.delays(5000), 1000);
}
#[test]
fn x552_swsm_never_granted_fails_closed() {
    let mut io = Fake::ready(0);
    io.regs.insert(SWSM, 1);
    assert_eq!(hardware::setup_link(&mut io, Family::X552, 0x15ab, 0), Err(Error::Semaphore { held: 1 }));
    assert_eq!(io.writes(), 0);
    assert_eq!(io.delays(50), 2000);
}
#[test]
fn x552_sideband_error_is_reported_and_semaphore_released() {
    let mut io = Fake::ready(0);
    io.iosf_error = true;
    assert!(matches!(hardware::setup_link(&mut io, Family::X552, 0x15ab, 0),
        Err(Error::Sideband { address: 0x420c, .. })));
    assert!(io.kr_writes.is_empty());
    released(&io);
}
#[test]
fn x552_sideband_busy_times_out() {
    let mut io = Fake::ready(0);
    io.regs.insert(IOSF_CTRL, 1 << 31);
    io.stuck = Some(IOSF_CTRL);
    assert!(matches!(hardware::setup_link(&mut io, Family::X552, 0x15ab, 0),
        Err(Error::Timeout { register: IOSF_CTRL, .. })));
    released(&io);
}

fn x557(io: &mut Fake, phy: u32, stalled: bool) {
    io.mdio.insert((phy, 1, 2), 0x0154);
    io.mdio.insert((phy, 1, 3), 0x0241);
    io.mdio.insert((phy, 1, 0xcc02), if stalled { 3 } else { 0 });
    io.mdio.insert((phy, 0x1e, 0xc479), 0x8012);
    for other in 0..32 { if other != phy { io.mdio.insert((other, 1, 2), 0xffff); } }
}
#[test]
fn x552_10gbase_t_finds_the_x557_releases_its_stall_and_forces_ixfi() {
    let mut io = Fake::ready(1);
    x557(&mut io, 3, true);
    io.regs.insert(0x4240, (1 << 16) | 1); // HLREG0 with MDCSPD
    io.kr.insert(0x820c, LC1_AN_ENABLE | (2 << 8));
    let setup = hardware::setup_link(&mut io, Family::X552, 0x15ad, 1).unwrap();
    assert_eq!(setup, Setup::X552(x552::Setup::Copper { phy: 3, id: 0x0154_0241, unstalled: true, internal: Internal::Ixfi }));
    assert_eq!(io.regs[&0x4240], 1, "MDIO clock slowed before the first access");
    let first_mdio = io.ops.iter().position(|op| matches!(op, Op::Write(MSCA, _))).unwrap();
    let hlreg = io.ops.iter().position(|op| matches!(op, Op::Write(0x4240, _))).unwrap();
    assert!(hlreg < first_mdio);
    assert_eq!(io.mdio_writes, [((3, 0x1e, 0xc479), 0x0012)]);
    // Forced 10G, AN off, training/FFE adaptation off, coefficient override, then restart.
    assert_eq!(io.kr[&0x820c], (4 << 8) | LC1_AN_RESTART);
    assert_eq!(io.kr[&0x8b00], 1 << 4);
    assert_eq!(io.kr[&0x9520], (1 << 31) | 0xe);
    assert!(io.kr_writes.iter().any(|&(a, _)| a == 0x8634));
    assert!(io.kr_writes.iter().any(|&(a, _)| a == 0x8638));
    assert_eq!(io.delays(100_000), 10, "MAC link wait bounded to 1 s");
    released(&io);

    // Copper up at 1G: the internal link is re-forced to 1G.
    io.mdio.insert((3, 7, 1), 1 << 2);
    io.mdio.insert((3, 7, 0xc800), 5);
    assert_eq!(x552::follow_copper(&mut io, 1, 3, Internal::Ixfi, 5000),
        Ok(Copper::Up { megabits: 1000, retrained: true }));
    assert_eq!(io.kr[&0x820c] & (7 << 8), 2 << 8);
    // At 10G nothing is re-forced.
    io.mdio.insert((3, 7, 0xc800), 7);
    let before = io.kr_writes.len();
    assert_eq!(x552::follow_copper(&mut io, 1, 3, Internal::Ixfi, 5000),
        Ok(Copper::Up { megabits: 10_000, retrained: false }));
    assert_eq!(io.kr_writes.len(), before);
    io.mdio.insert((3, 7, 0xc800), 3);
    assert_eq!(x552::follow_copper(&mut io, 1, 3, Internal::Ixfi, 5000),
        Ok(Copper::Unsupported { status: 3 }));
}
#[test]
fn x552_10gbase_t_uses_nvm_phy_address_and_kr_mode_and_no_second_unstall() {
    let mut io = Fake::ready(0);
    x557(&mut io, 5, false);
    io.regs.insert(0x11178, (1 << 24) | (5 << 3));
    let setup = hardware::setup_link(&mut io, Family::X552, 0x15ad, 0).unwrap();
    assert_eq!(setup, Setup::X552(x552::Setup::Copper { phy: 5, id: 0x0154_0241, unstalled: false, internal: Internal::Kr }));
    assert!(io.mdio_writes.is_empty());
    assert_eq!(io.kr[&0x420c], LC1_AN_ENABLE | LC1_KR | LC1_KX | LC1_AN_RESTART);
    // Only the NVM's address was probed.
    assert!(!io.ops.iter().any(|op| matches!(op, Op::Write(MSCA, v) if (v >> 21) & 0x1f != 5)));
}
#[test]
fn x552_10gbase_t_without_a_phy_fails_and_copper_wait_is_bounded() {
    let mut io = Fake::ready(0);
    assert_eq!(hardware::setup_link(&mut io, Family::X552, 0x15ad, 0), Err(Error::NoPhy));
    assert!(io.kr_writes.is_empty());
    let mut io = Fake::ready(0);
    x557(&mut io, 0, false);
    io.mdio.insert((0, 7, 1), 0);
    assert_eq!(x552::follow_copper(&mut io, 0, 0, Internal::Ixfi, 5000), Ok(Copper::Down));
    assert_eq!(io.delays(100_000), 50);
}

fn sfp(bytes: &[(usize, u8)]) -> Option<Vec<u8>> {
    let mut id = vec![0u8; 256];
    id[0] = 3;
    for &(i, v) in bytes { id[i] = v; }
    Some(id)
}
#[test]
fn x552_sfp_resets_cs4227_once_and_sets_passive_da_on_port_1() {
    let mut io = Fake::ready(1);
    io.i2c = Slave::cs4227(&[]);
    io.i2c.sfp = sfp(&[(8, 0x04)]);
    io.regs.insert(ESDP, (1 << 17) | (1 << 16) | (1 << 8) | 2);
    let setup = hardware::setup_link(&mut io, Family::X552, 0x15ac, 1).unwrap();
    let lc1 = LC1_AN_ENABLE | LC1_KR;
    assert_eq!(setup, Setup::X552(x552::Setup::Sfp {
        module: Module::DirectAttachPassive, cs4227_reset: true, link_ctrl: Some(lc1), edc: Some(2),
    }));
    assert_eq!(io.i2c.cs_resets, 1);
    assert_eq!(io.i2c.cs[&2], 0x5aa5, "reset recorded for the other port");
    assert_eq!(io.i2c.cs[&0x22b0], (2 << 1) | 1, "port 1 line side: CX1 EDC");
    assert_eq!(io.kr[&0x820c], lc1 | LC1_AN_RESTART);
    // Mux control: SDP0 and SDP1 GPIO, SDP1 output; mux released afterwards.
    assert_eq!(io.regs[&ESDP], 1 << 9);
    assert!(io.ops.contains(&Op::Write(ESDP, (1 << 9) | 2)), "mux selected while holding I2C");
    released(&io);
    assert_eq!(io.i2c.phase, Phase::Idle);

    // Second start (other instance already reset it): no second reset.
    let mut io2 = Fake::ready(0);
    io2.i2c = Slave::cs4227(&[(2, 0x5aa5)]);
    io2.i2c.sfp = sfp(&[(3, 0x10)]);
    let setup = hardware::setup_link(&mut io2, Family::X552, 0x15ac, 0).unwrap();
    assert_eq!(setup, Setup::X552(x552::Setup::Sfp {
        module: Module::Optical10g, cs4227_reset: false, link_ctrl: Some(lc1), edc: Some(4),
    }));
    assert_eq!(io2.i2c.cs_resets, 0);
    assert_eq!(io2.i2c.cs[&0x12b0], (4 << 1) | 1, "port 0 line side: SR EDC");
}
#[test]
fn x552_sfp_1g_optics_advertise_kx_only() {
    let mut io = Fake::ready(0);
    io.i2c = Slave::cs4227(&[(2, 0x5aa5)]);
    io.i2c.sfp = sfp(&[(6, 0x01)]);
    let setup = hardware::setup_link(&mut io, Family::X552, 0x15ac, 0).unwrap();
    assert_eq!(setup, Setup::X552(x552::Setup::Sfp {
        module: Module::Optical1g, cs4227_reset: false, link_ctrl: Some(LC1_AN_ENABLE | LC1_KX), edc: Some(4),
    }));
}
#[test]
fn x552_sfp_absent_or_unsupported_modules_are_not_set_up() {
    for (module, expect) in [
        (None, Module::Absent),
        (sfp(&[(6, 0x08)]), Module::Unsupported { identifier: 3, comp_10g: 0, comp_1g: 8, cable: 0 }),
        (sfp(&[(0, 0x0d), (3, 0x10)]), Module::Unsupported { identifier: 0x0d, comp_10g: 0x10, comp_1g: 0, cable: 0 }),
        (sfp(&[(8, 0x08)]), Module::Unsupported { identifier: 3, comp_10g: 0, comp_1g: 0, cable: 8 }),
    ] {
        let mut io = Fake::ready(0);
        io.i2c = Slave::cs4227(&[(2, 0x5aa5)]);
        io.i2c.sfp = module;
        let setup = hardware::setup_link(&mut io, Family::X552, 0x15ac, 0).unwrap();
        assert_eq!(setup, Setup::X552(x552::Setup::Sfp { module: expect, cs4227_reset: false, link_ctrl: None, edc: None }));
        assert!(io.kr_writes.is_empty());
        assert!(!io.i2c.cs.contains_key(&0x12b0));
        released(&io);
    }
}
#[test]
fn x552_cs4227_that_never_loads_fails() {
    let mut io = Fake::ready(0);
    io.i2c = Slave::cs4227(&[(0x181, 0)]);
    io.i2c.sfp = sfp(&[(3, 0x10)]);
    assert_eq!(hardware::setup_link(&mut io, Family::X552, 0x15ac, 0),
        Err(Error::Cs4227 { register: 0x181, value: 0 }));
    assert!(io.kr_writes.is_empty());
    released(&io);
}
