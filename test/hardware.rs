#[path = "../src/hardware.rs"]
mod hardware;
use hardware::f82599::{self, Laser, PhyReset};
use hardware::mdio::Speeds;
use hardware::sfp::Kind;
use hardware::x552::{self, Copper, Internal};
use hardware::{Error, Family, Link, Port, Prepared, Registers, Setup};
use std::collections::BTreeMap;

#[derive(Debug, PartialEq, Eq, Clone)]
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
    /// NVM words behind EERD.
    nvm: BTreeMap<u32, u16>,
    i2c: Slave,
}
const EERD: u32 = 0x10014;
const IOSF_CTRL: u32 = 0x11144;
const IOSF_DATA: u32 = 0x11148;
const MSCA: u32 = 0x425c;
const MSRWD: u32 = 0x4260;
const I2C_X552: u32 = 0x15f5c;
const I2C_82599: u32 = 0x28;
const SWSM: u32 = 0x10140;
const SWFW_SYNC: u32 = 0x10160;
const ESDP: u32 = 0x20;
const AUTOC: u32 = 0x42a0;
const AUTOC2: u32 = 0x42a8;
const LINKS: u32 = 0x42a4;
const CORECTL: u32 = 0x14f00;
const HICR: u32 = 0x15f00;
const FLEX_MNG: u32 = 0x15800;
const UP_10G: u32 = (1 << 30) | (3 << 28);

impl Fake {
    fn ready(lan: u32) -> Self {
        Self {
            regs: BTreeMap::from([
                (8, lan << 2), (0x10010, 0x300), (0x10110, 1 << (18 + lan)),
                (0x2f00, 8), (0xa200, 0x33221102), (0xa204, 0x80005544),
                (0x42b0, 0x0001_0000), // ANLP1: AN state machine running
            ]), ops: vec![], stuck: None, fail: None,
            kr: BTreeMap::new(), kr_writes: vec![], iosf_addr: 0, iosf_error: false,
            mdio: BTreeMap::new(), mdio_addr: (0, 0, 0), mdio_writes: vec![],
            nvm: BTreeMap::new(),
            i2c: Slave::default(),
        }
    }
    fn writes(&self) -> usize { self.ops.iter().filter(|op| matches!(op, Op::Write(..))).count() }
    fn writes_to(&self, reg: u32) -> Vec<u32> {
        self.ops.iter().filter_map(|op| match op { Op::Write(r, v) if *r == reg => Some(*v), _ => None }).collect()
    }
    fn delays(&self, micros: usize) -> usize { self.ops.iter().filter(|op| **op == Op::Delay(micros)).count() }
    fn reg(&self, reg: u32) -> u32 { self.regs.get(&reg).copied().unwrap_or(0) }
}
impl Registers for Fake {
    type Error = &'static str;
    fn read(&mut self, reg: u32) -> Result<u32, Self::Error> {
        self.ops.push(Op::Read(reg));
        if self.fail == Some(reg) { return Err("PCI read failed"); }
        let value = self.reg(reg);
        let (scl, sda) = (self.i2c.scl as u32, self.i2c.sda as u32);
        match reg {
            I2C_X552 => Ok(value & !((1 << 14) | (1 << 12)) | scl << 14 | sda << 12),
            I2C_82599 => Ok(value & !5 | scl | sda << 2),
            _ => Ok(value),
        }
    }
    fn write(&mut self, reg: u32, value: u32) -> Result<(), Self::Error> {
        self.ops.push(Op::Write(reg, value));
        if self.fail == Some(reg) { return Err("PCI write failed"); }
        if self.stuck != Some(reg) {
            self.regs.insert(reg, if reg == 0 { value & !((1 << 26) | 8) } else { value });
        } else if reg == 0 { self.regs.insert(reg, value); }
        match reg {
            EERD => {
                let word = (value >> 2) & 0x3fff;
                let data = *self.nvm.get(&word).unwrap_or(&0) as u32;
                self.regs.insert(EERD, data << 16 | word << 2 | 2);
            }
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
                        let mut data = (self.regs[&MSRWD] & 0xffff) as u16;
                        self.mdio_writes.push((self.mdio_addr, data));
                        // PHY XS soft reset: completes at once; the X557
                        // reports it in its vendor alarms.
                        let (phy, dev, r) = self.mdio_addr;
                        if dev == 4 && r == 0 && data & 0x8000 != 0 {
                            data &= !0x8000;
                            self.mdio.insert((phy, 1, 0xcc02), 3);
                        }
                        self.mdio.insert(self.mdio_addr, data);
                    }
                    _ => panic!("unexpected MDIO opcode"),
                }
                self.regs.insert(MSCA, value & !(1 << 30));
            }
            I2C_X552 => {
                let scl = value & ((1 << 9) | (1 << 13)) != 0;
                let sda = value & ((1 << 10) | (1 << 11)) != 0;
                self.i2c.drive(scl, sda);
            }
            I2C_82599 => self.i2c.drive(value & 2 != 0, value & 8 != 0),
            // X552 firmware, when HICR.EN is set: a shadow-RAM read (0x31)
            // of the big-endian byte address in dword 1 answers from `nvm`
            // in dword 3, clears C and sets SV.
            HICR if value & 3 == 3 && self.stuck != Some(HICR) => {
                assert_eq!(self.reg(FLEX_MNG), 0xff06_0031, "shadow RAM read command");
                assert_eq!(self.reg(FLEX_MNG + 8), 0x200, "length 2, big-endian");
                let word = self.reg(FLEX_MNG + 4).swap_bytes() / 2;
                let data = *self.nvm.get(&word).unwrap_or(&0) as u32;
                self.regs.insert(FLEX_MNG + 12, data);
                self.regs.insert(HICR, (value & !2) | 4);
            }
            _ => {}
        }
        Ok(())
    }
    fn delay_us(&mut self, micros: usize) { self.ops.push(Op::Delay(micros)); }
}

/// An I2C bus driven edge by edge from I2CCTL writes (either layout): the
/// SFP ID EEPROM (0xa0) and diagnostics page (0xa2), the X552 port expander
/// (0xe0), and the CS4227's checksummed 16-bit "combined" registers (0xbe).
#[derive(Default, PartialEq, Debug)]
enum Phase { #[default] Idle, Rx, RxAck, Tx, TxAck }
#[derive(Default)]
struct Slave {
    scl: bool, sda: bool, pull: bool,
    phase: Phase, shift: u8, n: u8, expect_addr: bool, reading: bool, acked: bool,
    txn: Vec<u8>, tx: Vec<u8>, cur: u8, sent: usize,
    sfp: Option<Vec<u8>>,
    diag: Vec<u8>,
    pe: [u8; 4],
    cs: BTreeMap<u16, u16>,
    cs_resets: usize,
    /// For each read the master ended with a NACK: (device, bytes sent).
    nacks: Vec<(u8, usize)>,
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
        s.diag = vec![0; 256];
        s
    }
    fn module(bytes: Option<Vec<u8>>) -> Self {
        Slave { sfp: bytes, diag: vec![0; 256], ..Default::default() }
    }
    fn present(&self, addr: u8) -> bool {
        match addr & 0xfe { 0xa0 | 0xa2 => self.sfp.is_some(), 0xe0 | 0xbe => true, _ => false }
    }
    fn drive(&mut self, scl: bool, master: bool) {
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
    fn next_tx(&mut self) -> u8 { self.sent += 1; if self.tx.is_empty() { 0xff } else { self.tx.remove(0) } }
    fn fall(&mut self) {
        match self.phase {
            Phase::Rx if self.n == 8 => {
                let byte = self.shift;
                if self.expect_addr {
                    self.expect_addr = false;
                    if !self.present(byte) { self.phase = Phase::Idle; return; }
                    self.reading = byte & 1 != 0;
                    self.txn.push(byte);
                    if self.reading { self.tx = self.respond(); self.sent = 0; }
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
                } else {
                    self.nacks.push((self.txn[0], self.sent));
                    self.phase = Phase::Idle; self.pull = false;
                }
            }
            _ => {}
        }
    }
    fn respond(&mut self) -> Vec<u8> {
        let t = &self.txn;
        match t[0] {
            0xa0 => self.sfp.as_ref().unwrap()[t[1] as usize..].to_vec(),
            0xa2 => self.diag[t[1] as usize..].to_vec(),
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
            0xa2 => { assert_eq!(t.len(), 3); self.diag[t[1] as usize] = t[2]; }
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

fn port(family: Family, device: u16, lan: u8) -> Port { Port { family, device, lan } }

/// The whole of Start's hardware sequence (spec 9): begin, veto, prepare,
/// reset, link setup.
fn run(io: &mut Fake, family: Family, device: u16) -> (Prepared, Result<Setup, Error<&'static str>>) {
    let lan = hardware::begin(io).unwrap();
    let p = port(family, device, lan);
    let veto = hardware::veto(io).unwrap();
    let prepared = hardware::prepare(io, p, veto).unwrap();
    hardware::reset(io, p).unwrap();
    (prepared, hardware::setup_link(io, p, veto, prepared))
}

fn released(io: &Fake) {
    assert_eq!(io.reg(SWFW_SYNC) & 0x1fff, 0, "SW_FW_SYNC software bits released");
    assert_eq!(io.reg(SWSM) & 3, 0, "SWSM.SMBI/SWESMBI released");
}

// ---- Common: quiesce, reset, NVM, LINKS -------------------------------------

#[test]
fn begin_quiesces_both_ports_before_any_reset() {
    for lan in 0..2 {
        let mut io = Fake::ready(lan);
        io.regs.insert(0x1028, 1 << 25);
        io.regs.insert(0xd028, 1 << 25);
        assert_eq!(hardware::begin(&mut io), Ok(lan as u8));
        assert!(io.ops.contains(&Op::Write(0x1028, 0)));
        assert!(io.ops.contains(&Op::Write(0xd028, 0)));
        let disable = io.ops.iter().position(|op| *op == Op::Write(0, 4)).unwrap();
        assert_eq!(&io.ops[disable + 1..disable + 3], &[Op::Read(0), Op::Read(8)]);
        assert!(!io.ops.iter().any(|op| matches!(op, Op::Write(0, v) if v & ((1 << 26) | 8) != 0)));
    }
}
#[test]
fn no_reset_with_outstanding_pcie_transactions() {
    let mut io = Fake::ready(0);
    io.regs.insert(8, 1 << 19);
    assert!(matches!(hardware::begin(&mut io), Err(Error::Timeout { register: 8, .. })));
    assert_eq!(io.delays(1000), 100);
}
#[test]
fn active_virtual_functions_are_not_touched() {
    let mut io = Fake::ready(0);
    io.regs.insert(8, 1 << 18);
    assert_eq!(hardware::begin(&mut io), Err(Error::VirtualizationActive));
    assert_eq!(io.writes(), 0);
}
#[test]
fn wedged_queue_fails_before_reset() {
    let mut io = Fake::ready(0);
    io.regs.insert(0xdfe8, 1 << 25); // Queue 127, not only queue zero.
    io.stuck = Some(0xdfe8);
    assert!(matches!(hardware::begin(&mut io), Err(Error::Timeout { register: 0xdfe8, .. })));
}
#[test]
fn reset_uses_lnk_rst_when_link_is_down_and_rst_when_up() {
    for (links, bit, lnk) in [(0, 1 << 3, true), (UP_10G, 1 << 26, false)] {
        let mut io = Fake::ready(1);
        io.regs.insert(LINKS, links);
        let id = hardware::reset(&mut io, port(Family::F82599, 0x1557, 1)).unwrap();
        assert_eq!(id.mac, [2, 0x11, 0x22, 0x33, 0x44, 0x55]);
        assert_eq!((id.lan, id.link_reset), (1, lnk));
        let ctrl = io.writes_to(0);
        assert_eq!(ctrl.len(), 1);
        assert_eq!(ctrl[0] & ((1 << 26) | 8), bit, "one reset type, never both");
        let at = io.ops.iter().position(|op| matches!(op, Op::Write(0, _))).unwrap();
        assert_eq!(&io.ops[at + 1..at + 4], &[Op::Delay(1000), Op::Read(0), Op::Delay(50_000)]);
        assert!(io.writes_to(SWFW_SYNC).is_empty(), "the 82599 reset takes no semaphore");
    }
}
#[test]
fn x540_reset_is_rst_under_the_phy_semaphore_then_100ms() {
    let mut io = Fake::ready(1);
    hardware::reset(&mut io, port(Family::X540, 0x1528, 1)).unwrap();
    let ctrl = io.ops.iter().position(|op| matches!(op, Op::Write(0, v) if v & (1 << 26) != 0)).unwrap();
    let taken = io.ops.iter().position(|op| *op == Op::Write(SWFW_SYNC, 1 << 2)).unwrap();
    assert!(taken < ctrl, "PHY1 held across the reset");
    assert!(io.ops[ctrl..].contains(&Op::Delay(100_000)));
    released(&io);
}
#[test]
fn x552_sfp_reset_holds_the_shared_i2c_mask_and_redoes_the_mux() {
    let mut io = Fake::ready(1);
    io.regs.insert(ESDP, (1 << 17) | (1 << 16) | (1 << 8));
    hardware::reset(&mut io, port(Family::X552, 0x15ac, 1)).unwrap();
    assert!(io.ops.contains(&Op::Write(SWFW_SYNC, 0x1806)));
    assert!(io.writes_to(ESDP).iter().any(|v| v & 2 != 0), "mux on while held");
    assert_eq!(io.reg(ESDP), 1 << 9, "mux set-up redone after reset");
    released(&io);
}
#[test]
fn reset_and_nvm_waits_are_bounded() {
    for reg in [0, 0x10010, 0x2f00] {
        let mut io = Fake::ready(0);
        if reg == 0 { io.stuck = Some(0); }
        else { io.regs.insert(reg, 0); }
        assert!(matches!(hardware::reset(&mut io, port(Family::F82599, 0x1557, 0)),
            Err(Error::Timeout { register, .. }) if register == reg));
        assert!(!io.ops.contains(&Op::Read(0xa200)));
        assert!(io.ops.len() < 3000);
    }
}
#[test]
fn cfg_done_that_never_sets_is_reported_not_fatal() {
    // server3 (X9, 8086:1557, #21): EEMNGCTL reads 0x80000196, no CFG_DONE.
    for lan in [0, 1] {
        let mut io = Fake::ready(lan);
        io.regs.insert(0x10110, 0x8000_0196 | (1 << (19 - lan))); // the other port's bit only
        let id = hardware::reset(&mut io, port(Family::F82599, 0x1557, lan as u8)).unwrap();
        assert_eq!(id.cfg_pending, Some(0x8000_0196 | (1 << (19 - lan))));
        assert_eq!(id.mac, [2, 0x11, 0x22, 0x33, 0x44, 0x55]);
        let polls = io.ops.iter().filter(|op| **op == Op::Read(0x10110)).count();
        assert_eq!(polls, 1001, "bounded to 1 s");
        assert!(io.ops.contains(&Op::Read(0x2f00)), "RDRXCTL wait still runs");
    }
    for lan in [0, 1] {
        let mut io = Fake::ready(lan);
        let id = hardware::reset(&mut io, port(Family::F82599, 0x1557, lan as u8)).unwrap();
        assert_eq!(id.cfg_pending, None);
    }
}
#[test]
fn absent_nvm_and_invalid_macs_fail() {
    let p = port(Family::F82599, 0x10fb, 0);
    let mut io = Fake::ready(0);
    io.regs.insert(0x10010, 1 << 9);
    assert_eq!(hardware::reset(&mut io, p), Err(Error::MissingNvm));
    for (low, high) in [(0, 0x80000000), (0xffffffff, 0x8000ffff), (3, 0x80000000), (2, 0)] {
        let mut io = Fake::ready(0);
        io.regs.insert(0xa200, low); io.regs.insert(0xa204, high);
        assert_eq!(hardware::reset(&mut io, p), Err(Error::InvalidMac));
    }
}
#[test]
fn transport_errors_propagate() {
    for reg in [0x888, 0x3000, 0x1028] {
        let mut io = Fake::ready(0); io.fail = Some(reg);
        assert!(matches!(hardware::begin(&mut io), Err(Error::Io(_))));
    }
    for reg in [0, 0x10010, 0x10110] {
        let mut io = Fake::ready(0); io.fail = Some(reg);
        assert!(matches!(hardware::reset(&mut io, port(Family::F82599, 0x10fb, 0)), Err(Error::Io(_))));
        assert!(!io.ops.contains(&Op::Read(0xa200)));
    }
}
#[test]
fn nvm_words_through_eerd() {
    let mut io = Fake::ready(0);
    io.nvm.insert(0x2b, 0xbeef);
    assert_eq!(hardware::nvm_word(&mut io, 0x2b), Ok(0xbeef));
    assert_eq!(io.writes_to(EERD), [0x2b << 2 | 1]);
}
#[test]
fn links_speeds_2g5_on_x552_and_removed_device() {
    let mut io = Fake::ready(0);
    for (speed, mbps) in [(0, None), (1, Some(100)), (2, Some(1000)), (3, Some(10000))] {
        io.regs.insert(LINKS, (1 << 30) | (speed << 28));
        assert_eq!(hardware::link(&mut io, Family::F82599), Ok(Link::Up { megabits: mbps }));
    }
    io.regs.insert(LINKS, UP_10G | (1 << 27));
    assert_eq!(hardware::link(&mut io, Family::X552), Ok(Link::Up { megabits: Some(2500) }));
    assert_eq!(hardware::link(&mut io, Family::F82599), Ok(Link::Up { megabits: Some(10000) }));
    io.regs.insert(LINKS, (3 << 28) | (1 << 7));
    assert_eq!(hardware::link(&mut io, Family::X540), Ok(Link::Down));
    io.fail = Some(LINKS);
    assert_eq!(hardware::link(&mut io, Family::X540), Err(Error::Io("PCI read failed")));
    io.fail = None; io.regs.insert(LINKS, u32::MAX);
    assert_eq!(hardware::link(&mut io, Family::X540), Err(Error::Removed));
}
#[test]
fn link_wait_polls_every_100ms_and_is_bounded() {
    let p = port(Family::F82599, 0x10f7, 0);
    let mut io = Fake::ready(0);
    let w = hardware::wait_link(&mut io, p, None, 3000).unwrap();
    assert_eq!(w.link, Link::Down);
    assert_eq!(io.delays(100_000), 30);
    let mut io = Fake::ready(0);
    io.regs.insert(LINKS, UP_10G);
    assert_eq!(hardware::wait_link(&mut io, p, None, 3000).unwrap().link, Link::Up { megabits: Some(10_000) });
    assert_eq!(io.delays(100_000), 0);
    assert_eq!(hardware::link_budget_ms(p), 3000);
    assert_eq!(hardware::link_budget_ms(port(Family::X540, 0x1528, 0)), 9000);
    assert_eq!(hardware::link_budget_ms(port(Family::X552, 0x15ad, 0)), 9000);
    assert_eq!(hardware::link_budget_ms(port(Family::F82599, 0x151c, 0)), 9000);
}
#[test]
fn link_mode_names_each_lms() {
    let lms = |v: u32| v << 13;
    assert_eq!(hardware::link_mode(lms(0), 0), "1G SFI");
    assert_eq!(hardware::link_mode(lms(0) | (1 << 9), 0), "1G KX/BX, no AN");
    assert_eq!(hardware::link_mode(lms(1) | (1 << 7), 0), "10G KX4, no AN");
    assert_eq!(hardware::link_mode(lms(3), 2 << 16), "10G SFI");
    assert_eq!(hardware::link_mode(lms(3), 0), "10G KR, no AN");
    assert_eq!(hardware::link_mode(lms(4), 0), "KX/KX4/KR AN");
    assert_eq!(hardware::link_mode(lms(7), 0), "KX/KX4/KR AN + SGMII");
}

// ---- Semaphores ---------------------------------------------------------------

#[test]
fn x82599_semaphore_uses_swesmbi_and_fails_closed_after_200_tries() {
    let p = port(Family::F82599, 0x10fb, 0);
    let mut io = Fake::ready(0);
    hardware::sync::acquire(&mut io, p, 2).unwrap();
    assert!(io.writes_to(SWSM).contains(&2), "SWESMBI requested");
    assert_eq!(io.reg(SWFW_SYNC), 2);
    hardware::sync::release(&mut io, p, 2).unwrap();
    released(&io);
    let mut io = Fake::ready(0);
    io.regs.insert(SWFW_SYNC, 2 << 5); // firmware holds PHY0
    assert_eq!(hardware::sync::acquire(&mut io, p, 2), Err(Error::Semaphore { held: 0x40 }));
    assert_eq!(io.delays(5000), 200);
    assert_eq!(io.reg(SWFW_SYNC), 0x40, "firmware's bit left alone");
}
#[test]
fn x540_semaphore_gives_up_after_200_tries_and_x552_after_1000() {
    for (family, tries) in [(Family::X540, 200), (Family::X552, 1000)] {
        let mut io = Fake::ready(0);
        io.regs.insert(SWFW_SYNC, 2 << 5);
        assert_eq!(hardware::sync::acquire(&mut io, port(family, 0, 0), 2), Err(Error::Semaphore { held: 0x40 }));
        assert_eq!(io.delays(5000), tries);
    }
}
#[test]
fn swsm_never_granted_fails_closed() {
    let mut io = Fake::ready(0);
    io.regs.insert(SWSM, 1);
    assert_eq!(hardware::sync::acquire(&mut io, port(Family::X552, 0x15ab, 0), 6), Err(Error::Semaphore { held: 1 }));
    assert_eq!(io.writes(), 0);
    assert_eq!(io.delays(50), 2000);
}

// ---- 82599 ----------------------------------------------------------------------

fn sfp(bytes: &[(usize, u8)]) -> Option<Vec<u8>> {
    let mut id = vec![0u8; 256];
    id[0] = 3;
    for &(i, v) in bytes { id[i] = v; }
    Some(id)
}
/// An 82599 SFP+ port as the NVM leaves it: LMS 011 (10G SFI) with KX4/KX
/// advertised, AUTOC2 SFI, SDP3 an output driving TX_DISABLE, and an NVM
/// with init sequences for SR/LR (5, 6) and DA (3, 4).
fn x82599(lan: u32, module: Option<Vec<u8>>) -> Fake {
    let mut io = Fake::ready(lan);
    io.regs.insert(AUTOC, (3 << 30) | (3 << 13));
    io.regs.insert(AUTOC2, 2 << 16);
    io.regs.insert(ESDP, (1 << 11) | (1 << 3));
    io.i2c = Slave::module(module);
    for (w, v) in [(0x2b, 0x100), (0x2c, 0x80),
        (0x101, 3), (0x102, 0x180), (0x103, 5), (0x104, 0x200), (0x105, 6), (0x106, 0x200), (0x107, 4), (0x108, 0x180), (0x109, 0xffff),
        (0x180, 0), (0x181, 0x0d0d), (0x182, 0xffff),
        (0x200, 0), (0x201, 0x1111), (0x202, 0x2222), (0x203, 0xffff)] {
        io.nvm.insert(w, v);
    }
    io
}
#[test]
fn x82599_sfp_sr_runs_the_nvm_sequence_and_enters_sfi_with_a_pipeline_reset() {
    let mut io = x82599(0, sfp(&[(3, 0x10)]));
    let (prepared, setup) = run(&mut io, Family::F82599, 0x1557);
    let Prepared::F82599(f82599::Prepared::Module(m)) = prepared else { panic!("{prepared:?}") };
    assert_eq!((m.kind, m.multispeed), (Kind::SrLr, false));
    let Setup::F82599(f82599::Setup::Module { sequence, laser, speed, crosstalk, autoc, .. }) = setup.unwrap() else { panic!() };
    assert_eq!((sequence, laser, speed, crosstalk), (Some(2), Laser::On, None, false));
    // CORECTL gets the words after the first, in order, holding MAC_CSR.
    assert_eq!(io.writes_to(CORECTL), [0x1111, 0x2222]);
    let csr = io.ops.iter().position(|op| *op == Op::Write(SWFW_SYNC, 8)).unwrap();
    let first = io.ops.iter().position(|op| *op == Op::Write(CORECTL, 0x1111)).unwrap();
    assert!(csr < first);
    // Protected write of original | LMS 011, then the pipeline reset: LMS
    // bit 2 toggled with Restart_AN, back to the value with Restart_AN.
    let orig = (3 << 30) | (3 << 13);
    let sfi = orig; // LMS 011 already: the OR changes nothing
    assert_eq!(io.writes_to(AUTOC), [sfi, (sfi | (1 << 12)) ^ (1 << 15), sfi | (1 << 12)]);
    assert_eq!(autoc, sfi | (1 << 12));
    assert!(io.delays(10_000) >= 1, "10 ms after the init sequence");
    // Laser on: SDP3 cleared, then 100 ms.
    assert_eq!(io.reg(ESDP) & (1 << 3), 0);
    assert!(io.ops.contains(&Op::Delay(100_000)));
    released(&io);
}
#[test]
fn x82599_da_is_multispeed_with_hard_rate_select_and_a_laser_flap() {
    let mut io = x82599(1, sfp(&[(8, 0x04)]));
    io.regs.insert(LINKS, UP_10G);
    let (_, setup) = run(&mut io, Family::F82599, 0x10fb);
    let Setup::F82599(f82599::Setup::Module { module, speed, sequence, .. }) = setup.unwrap() else { panic!() };
    assert_eq!((module.kind, module.multispeed, speed, sequence), (Kind::DaCu, true, Some(10_000), Some(1)));
    assert_eq!(io.writes_to(CORECTL), [0x0d0d], "passive DA on core 1: key 4");
    let esdp = io.reg(ESDP);
    assert_eq!(esdp & ((1 << 13) | (1 << 5)), (1 << 13) | (1 << 5), "SDP5 output, high for 10G");
    assert_eq!(esdp & (1 << 3), 0, "laser left on");
    assert!(io.writes_to(ESDP).iter().filter(|v| *v & (1 << 3) != 0).count() >= 1, "flap: laser off once");
}
#[test]
fn x82599_1g_module_moves_sfi_to_1g_with_an() {
    let mut io = x82599(0, sfp(&[(6, 0x01)]));
    let (_, setup) = run(&mut io, Family::F82599, 0x10fb);
    let Setup::F82599(f82599::Setup::Module { autoc, .. }) = setup.unwrap() else { panic!() };
    assert_eq!((autoc >> 13) & 7, 2, "LMS 010: 1G with AN");
}
#[test]
fn x82599_unknown_absent_and_unlisted_modules() {
    // No module: 11 attempts, 100 ms after each failure; nothing set up.
    let mut io = x82599(0, None);
    let (prepared, setup) = run(&mut io, Family::F82599, 0x1557);
    assert!(matches!(prepared, Prepared::F82599(f82599::Prepared::Module(m)) if m.kind == Kind::NotPresent));
    assert_eq!(io.delays(100_000), 11);
    assert!(matches!(setup, Ok(Setup::F82599(f82599::Setup::Module { sequence: None, .. }))));
    assert!(io.writes_to(AUTOC).is_empty() && io.writes_to(CORECTL).is_empty());
    // An SR module with no NVM init sequence for it.
    let mut io = x82599(0, sfp(&[(3, 0x10)]));
    io.nvm.insert(0x103, 0xffff);
    assert_eq!(run(&mut io, Family::F82599, 0x1557).1, Err(Error::NoInitSequence { key: 5 }));
    // Active DA that is not limiting is unknown and refused.
    let mut io = x82599(0, sfp(&[(8, 0x08)]));
    let (prepared, _) = run(&mut io, Family::F82599, 0x1557);
    assert!(matches!(prepared, Prepared::F82599(f82599::Prepared::Module(m)) if m.kind == Kind::Unknown));
    assert!(io.writes_to(AUTOC).is_empty());
}
#[test]
fn x82599_veto_writes_no_autoc_and_lesm_takes_mac_csr() {
    let mut io = x82599(0, sfp(&[(3, 0x10)]));
    io.regs.insert(0x42d0, 1);
    run(&mut io, Family::F82599, 0x1557).1.unwrap();
    assert!(io.writes_to(AUTOC).is_empty());
    let mut io = x82599(0, sfp(&[(3, 0x10)]));
    for (w, v) in [(0x0f, 0x300), (0x302, 0x400), (0x401, 0x8000)] { io.nvm.insert(w, v); }
    run(&mut io, Family::F82599, 0x1557).1.unwrap();
    let autoc = io.ops.iter().position(|op| matches!(op, Op::Write(AUTOC, _))).unwrap();
    let taken = io.ops[..autoc].iter().rposition(|op| matches!(op, Op::Write(SWFW_SYNC, _))).unwrap();
    assert_eq!(io.ops[taken], Op::Write(SWFW_SYNC, 8), "MAC_CSR held for the AUTOC write under LESM");
    released(&io);
}
#[test]
fn x82599_laser_is_left_to_manageability_or_an_input_sdp3() {
    let mut io = x82599(0, sfp(&[(3, 0x10)]));
    io.regs.insert(0x10148, 0x4);
    io.regs.insert(0x5820, 1 << 17);
    let (_, s) = run(&mut io, Family::F82599, 0x1557);
    assert!(matches!(s, Ok(Setup::F82599(f82599::Setup::Module { laser: Laser::Manageability, .. }))));
    assert_ne!(io.reg(ESDP) & (1 << 3), 0);
    let mut io = x82599(0, sfp(&[(3, 0x10)]));
    io.regs.insert(ESDP, 1 << 3);
    let (_, s) = run(&mut io, Family::F82599, 0x1557);
    assert!(matches!(s, Ok(Setup::F82599(f82599::Setup::Module { laser: Laser::NoDirection, .. }))));
}
#[test]
fn x82599_crosstalk_fix_reads_cage_presence_and_confirms_link() {
    let mut io = Fake::ready(0);
    io.regs.insert(LINKS, UP_10G);
    assert_eq!(f82599::link(&mut io, true), Ok(Link::Down), "empty cage");
    io.regs.insert(ESDP, 1 << 2);
    assert_eq!(f82599::link(&mut io, true), Ok(Link::Up { megabits: Some(10_000) }));
    assert_eq!(io.delays(5000), 1);
    let mut io = x82599(0, sfp(&[(3, 0x10)]));
    io.nvm.insert(0x2c, 0);
    let (_, s) = run(&mut io, Family::F82599, 0x1557);
    assert!(matches!(s, Ok(Setup::F82599(f82599::Setup::Module { crosstalk: true, .. }))));
}
#[test]
fn x82599_backplane_keeps_the_nvm_advertisement() {
    let mut io = Fake::ready(0);
    let an = (4 << 13) | (1 << 16) | (1 << 30) | (1 << 31);
    io.regs.insert(AUTOC, an);
    let (_, s) = run(&mut io, Family::F82599, 0x10f8);
    assert_eq!(s, Ok(Setup::F82599(f82599::Setup::Backplane { autoc: an, autoc2: 0, written: false, an_complete: None })));
    assert!(io.writes_to(AUTOC).is_empty());
}
#[test]
fn x82599_t3_resets_and_advertises_the_tn1010() {
    let mut io = Fake::ready(0);
    io.mdio.insert((2, 1, 2), 0x00a1);
    io.mdio.insert((2, 1, 3), 0x9411);
    io.mdio.insert((2, 1, 4), 0x0011);
    io.mdio.insert((2, 1, 0x9005), 0);
    for a in 0..2 { io.mdio.insert((a, 1, 2), 0xffff); }
    io.mdio.insert((2, 7, 0x10), 0x0100);
    let (prepared, s) = run(&mut io, Family::F82599, 0x151c);
    assert_eq!(prepared, Prepared::F82599(f82599::Prepared::Copper { phy: 2, id: 0x00a1_9410, reset: PhyReset::Done }));
    assert!(io.mdio_writes.contains(&((2, 4, 0), 0x8000)));
    assert_eq!(io.mdio[&(2, 7, 0x20)] & (1 << 12), 1 << 12);
    assert_eq!(io.mdio[&(2, 7, 0x17)] & (1 << 14), 1 << 14);
    assert_eq!(io.mdio[&(2, 7, 0x10)] & (1 << 8), 0);
    assert_eq!(io.mdio[&(2, 7, 0)] & (1 << 9), 1 << 9);
    assert_eq!(io.writes_to(AUTOC).len(), 2, "MAC side: pipeline reset");
    assert!(matches!(s, Ok(Setup::F82599(f82599::Setup::Copper { restarted: true, .. }))));
    // Over-temperature: no PHY reset.
    let mut io = Fake::ready(0);
    io.mdio.insert((0, 1, 2), 0x00a1);
    io.mdio.insert((0, 1, 3), 0x9410);
    io.mdio.insert((0, 1, 0x9005), 1);
    let (prepared, _) = run(&mut io, Family::F82599, 0x151c);
    assert!(matches!(prepared, Prepared::F82599(f82599::Prepared::Copper { reset: PhyReset::OverTemperature, .. })));
    assert!(!io.mdio_writes.iter().any(|&(k, _)| k == (0, 4, 0)));
}
#[test]
fn x82599_qsfp_requests_the_shared_bus() {
    let mut io = Fake::ready(0);
    io.regs.insert(ESDP, 1 << 1); // SDP1: bus granted
    io.i2c = Slave::module(sfp(&[(0, 0x0d), (0x83, 0x10)]));
    let lan = hardware::begin(&mut io).unwrap();
    let p = port(Family::F82599, 0x1558, lan);
    let prepared = hardware::prepare(&mut io, p, false).unwrap();
    assert!(matches!(prepared, Prepared::F82599(f82599::Prepared::Module(m)) if m.kind == Kind::SrLr));
    assert!(io.writes_to(ESDP).iter().any(|v| v & 1 != 0), "bus requested on SDP0");
    assert_eq!(io.reg(ESDP) & 1, 0, "and released");
    // No grant: an I2C failure after 200 × 5 ms, so no module.
    let mut io = Fake::ready(0);
    io.i2c = Slave::module(sfp(&[(0, 0x0d)]));
    let prepared = hardware::prepare(&mut io, p, false).unwrap();
    assert!(matches!(prepared, Prepared::F82599(f82599::Prepared::Module(m)) if m.kind == Kind::NotPresent));
    assert_eq!(io.delays(5000), 200);
}

/// An 82599 QSFP+ port (1558): the shared bus always granted (SDP1).
fn qsfp(module: &[(usize, u8)]) -> Fake {
    let mut q = vec![(0, 0x0d)];
    q.extend_from_slice(module);
    let mut io = x82599(0, sfp(&q));
    io.regs.insert(ESDP, 1 << 1);
    io
}
#[test]
fn x82599_qsfp_sr_sx_is_multispeed_without_rate_select() {
    let mut io = qsfp(&[(0x83, 0x10), (0x86, 0x01)]);
    io.regs.insert(LINKS, UP_10G);
    let (prepared, setup) = run(&mut io, Family::F82599, 0x1558);
    assert!(matches!(prepared, Prepared::F82599(f82599::Prepared::Module(m)) if m.kind == Kind::SrLr && m.multispeed));
    let Setup::F82599(f82599::Setup::Module { speed, laser, sequence, crosstalk, .. }) = setup.unwrap() else { panic!() };
    assert_eq!((speed, laser, sequence, crosstalk), (Some(10_000), Laser::None, Some(2), false));
    assert!(io.writes_to(ESDP).iter().all(|v| v & ((1 << 13) | (1 << 5) | (1 << 3)) == 0), "no rate select, no laser");
    // No link at 10G: 1G with no AN (QSFP never auto-negotiates), then back to 10G.
    let mut io = qsfp(&[(0x83, 0x20), (0x86, 0x02)]);
    let (_, setup) = run(&mut io, Family::F82599, 0x1558);
    let Setup::F82599(f82599::Setup::Module { module, speed, autoc, .. }) = setup.unwrap() else { panic!() };
    assert_eq!((module.multispeed, speed, (autoc >> 13) & 7), (true, None, 3));
    assert!(io.writes_to(AUTOC).iter().any(|v| (v >> 13) & 7 == 0), "1G tried as LMS 000");
    // DA and SR-only QSFP modules are single speed.
    for bytes in [[(0x83, 0x08), (0x86, 0x01)], [(0x83, 0x10), (0x86, 0x02)]] {
        let mut io = qsfp(&bytes);
        let (prepared, _) = run(&mut io, Family::F82599, 0x1558);
        assert!(matches!(prepared, Prepared::F82599(f82599::Prepared::Module(m)) if !m.multispeed), "{bytes:02x?}");
    }
}
#[test]
fn x82599_qsfp_gets_the_crosstalk_cage_check() {
    let mut io = qsfp(&[(0x83, 0x10)]);
    io.nvm.insert(0x2c, 0);
    io.regs.insert(LINKS, UP_10G);
    let (_, setup) = run(&mut io, Family::F82599, 0x1558);
    let setup = setup.unwrap();
    assert!(matches!(setup, Setup::F82599(f82599::Setup::Module { crosstalk: true, .. })));
    let p = port(Family::F82599, 0x1558, 0);
    assert_eq!(hardware::wait_link(&mut io, p, Some(&setup), 0).unwrap().link, Link::Down, "SDP2 clear: empty cage");
    let esdp = io.reg(ESDP);
    io.regs.insert(ESDP, esdp | (1 << 2));
    assert_eq!(hardware::wait_link(&mut io, p, Some(&setup), 0).unwrap().link, Link::Up { megabits: Some(10_000) });
}
#[test]
fn x82599_ls_154f_runs_the_nvm_autoc_like_a_backplane() {
    assert_eq!(f82599::media(0x154f), f82599::Media::Lco);
    let mut io = Fake::ready(0);
    let sfi = (3 << 13) | (1 << 31);
    io.regs.insert(AUTOC, sfi);
    io.regs.insert(AUTOC2, 2 << 16);
    let (prepared, s) = run(&mut io, Family::F82599, 0x154f);
    assert_eq!(prepared, Prepared::F82599(f82599::Prepared::Backplane));
    assert_eq!(s, Ok(Setup::F82599(f82599::Setup::Backplane { autoc: sfi, autoc2: 2 << 16, written: false, an_complete: None })));
    assert!(io.writes_to(AUTOC).is_empty());
    assert!(io.writes_to(I2C_82599).is_empty(), "no module identification");
    assert!(io.writes_to(ESDP).is_empty(), "no laser or rate select");
    assert_eq!(hardware::link_budget_ms(port(Family::F82599, 0x154f, 0)), 3000);
}

// ---- X540 -----------------------------------------------------------------------

#[test]
fn x540_powers_the_phy_on_advertises_and_restarts_unless_vetoed() {
    for veto in [false, true] {
        let mut io = Fake::ready(0);
        io.mdio.insert((1, 1, 2), 0x0154);
        io.mdio.insert((1, 1, 3), 0x0203);
        io.mdio.insert((0, 1, 2), 0);
        io.mdio.insert((1, 1, 4), 0x0031);
        io.mdio.insert((1, 0x1e, 0), 0x0840);
        io.mdio.insert((1, 7, 0x10), 0x0080);
        io.mdio.insert((1, 7, 0), 0);
        if veto { io.regs.insert(0x42d0, 1); }
        let (_, s) = run(&mut io, Family::X540, 0x1528);
        let all = Speeds { g10: true, g1: true, m100: true };
        assert_eq!(s, Ok(Setup::X540(hardware::x540::Setup { phy: 1, id: 0x0154_0200, advertised: all, restarted: !veto })));
        assert_eq!(io.mdio[&(1, 0x1e, 0)], 0x0040, "low-power bit cleared");
        assert_eq!(io.mdio[&(1, 7, 0x20)] & (1 << 12), 1 << 12);
        assert_eq!(io.mdio[&(1, 7, 0xc400)] & (1 << 15), 1 << 15);
        assert_eq!(io.mdio[&(1, 7, 0x10)] & 0x180, 0x100);
        assert_eq!(io.mdio[&(1, 7, 0)] & (1 << 9), if veto { 0 } else { 1 << 9 });
        released(&io);
    }
}

// ---- X552 -----------------------------------------------------------------------

const LC1_AN_ENABLE: u32 = 1 << 29;
const LC1_AN_RESTART: u32 = 1 << 31;
const LC1_KR: u32 = 1 << 18;
const LC1_KX: u32 = 1 << 16;

#[test]
fn x552_kr_advertises_kr_and_kx_and_restarts_an_on_its_own_port() {
    for (lan, reg, other) in [(0u32, 0x420c, 0x820c), (1, 0x820c, 0x420c)] {
        let mut io = Fake::ready(lan);
        io.kr.insert(reg, (1 << 24) | (4 << 8)); // unrelated bits kept, stale forced speed too
        let (_, setup) = run(&mut io, Family::X552, 0x15ab);
        let lc1 = (1 << 24) | (4 << 8) | LC1_AN_ENABLE | LC1_KR | LC1_KX;
        assert_eq!(setup, Ok(Setup::X552(x552::Setup::Kr { link_ctrl: lc1 })));
        assert_eq!(io.kr_writes, [(reg, lc1), (reg, lc1 | LC1_AN_RESTART)]);
        assert!(!io.kr.contains_key(&other));
        assert!(io.ops.contains(&Op::Write(IOSF_CTRL, reg)));
        released(&io);
    }
}
#[test]
fn x552_kr_leaves_link_to_manageability_on_veto() {
    let mut io = Fake::ready(0);
    io.regs.insert(0x42d0, 1);
    let (_, setup) = run(&mut io, Family::X552, 0x15ab);
    assert_eq!(setup, Ok(Setup::X552(x552::Setup::ManageabilityVeto)));
    assert!(io.kr_writes.is_empty());
}
#[test]
fn x552_kx4_xfi_and_1g_t_write_nothing() {
    for (device, expect) in [(0x15aa, x552::Setup::Kx4), (0x15b0, x552::Setup::Xfi), (0x15ae, x552::Setup::FirmwarePhy)] {
        let mut io = Fake::ready(0);
        let p = port(Family::X552, device, 0);
        assert_eq!(hardware::prepare(&mut io, p, false), Ok(Prepared::X552(x552::Prepared::Nothing)));
        assert_eq!(hardware::setup_link(&mut io, p, false, Prepared::X552(x552::Prepared::Nothing)), Ok(Setup::X552(expect)));
        assert!(io.ops.is_empty());
    }
}
#[test]
fn x552_sideband_errors_and_busy_are_reported_and_semaphore_released() {
    let p = port(Family::X552, 0x15ab, 0);
    let mut io = Fake::ready(0);
    io.iosf_error = true;
    assert!(matches!(hardware::setup_link(&mut io, p, false, Prepared::X552(x552::Prepared::Nothing)),
        Err(Error::Sideband { address: 0x420c, .. })));
    assert!(io.kr_writes.is_empty());
    released(&io);
    let mut io = Fake::ready(0);
    io.regs.insert(IOSF_CTRL, 1 << 31);
    io.stuck = Some(IOSF_CTRL);
    assert!(matches!(hardware::setup_link(&mut io, p, false, Prepared::X552(x552::Prepared::Nothing)),
        Err(Error::Timeout { register: IOSF_CTRL, .. })));
    released(&io);
}

fn x557(io: &mut Fake, phy: u32, stalled: bool) {
    io.mdio.insert((phy, 1, 2), 0x0154);
    io.mdio.insert((phy, 1, 3), 0x0241);
    io.mdio.insert((phy, 1, 4), 0x0031);
    io.mdio.insert((phy, 1, 0xcc02), if stalled { 3 } else { 0 });
    io.mdio.insert((phy, 0x1e, 0xc479), 0x8012);
    io.mdio.insert((phy, 7, 1), 0);
    io.mdio.insert((phy, 7, 0x10), 0x0180);
}
#[test]
fn x552_10gbase_t_unstalls_resets_forces_ixfi_and_advertises() {
    let mut io = Fake::ready(1);
    x557(&mut io, 3, true);
    io.regs.insert(0x4240, (1 << 16) | 1); // HLREG0 with MDCSPD
    io.kr.insert(0x820c, LC1_AN_ENABLE | (2 << 8));
    let (prepared, setup) = run(&mut io, Family::X552, 0x15ad);
    assert_eq!(prepared, Prepared::X552(x552::Prepared::Copper {
        phy: 3, id: 0x0154_0240, sel: 0, unstalled: true, reset: true, internal: Internal::Ixfi }));
    assert_eq!(io.reg(0x4240), 1, "MDIO clock slowed");
    let hlreg = io.ops.iter().position(|op| matches!(op, Op::Write(0x4240, _))).unwrap();
    let first_mdio = io.ops.iter().position(|op| matches!(op, Op::Write(MSCA, _))).unwrap();
    assert!(hlreg < first_mdio);
    // Unstall before the PHY reset, both before the MAC reset.
    let unstall = io.mdio_writes.iter().position(|w| *w == ((3, 0x1e, 0xc479), 0x0012)).unwrap();
    let phy_reset = io.mdio_writes.iter().position(|w| *w == ((3, 4, 0), 0x8000)).unwrap();
    assert!(unstall < phy_reset);
    let mac_reset = io.ops.iter().position(|op| matches!(op, Op::Write(0, v) if v & 8 != 0)).unwrap();
    let last_reset_mdio = io.ops.iter().rposition(|op| *op == Op::Write(MSRWD, 0x8000)).unwrap();
    assert!(last_reset_mdio < mac_reset);
    // iXFI forced 10G: AN off, training/FFE adaptation off, coefficient override, restart.
    assert_eq!(io.kr[&0x820c], (4 << 8) | LC1_AN_RESTART);
    assert_eq!(io.kr[&0x8b00], 1 << 4);
    assert_eq!(io.kr[&0x9520], (1 << 31) | 0xe);
    // X557 advertises 10G + 1G, 100M removed, then AN restart.
    let Ok(Setup::X552(x552::Setup::Copper { advertised, restarted, .. })) = setup else { panic!("{setup:?}") };
    assert_eq!((advertised, restarted), (Speeds { g10: true, g1: true, m100: false }, true));
    assert_eq!(io.mdio[&(3, 7, 0x10)] & 0x180, 0);
    assert_eq!(io.mdio[&(3, 7, 0xc400)] & (1 << 15), 1 << 15);
    assert_eq!(io.mdio[&(3, 7, 0)] & (1 << 9), 1 << 9);
    released(&io);
}
#[test]
fn x552_10gbase_t_link_needs_links_and_copper_and_reforces_on_speed() {
    let p = port(Family::X552, 0x15ad, 1);
    let setup = Setup::X552(x552::Setup::Copper {
        phy: 3, id: 0x0154_0240, sel: 0, unstalled: false, reset: true, internal: Internal::Ixfi,
        advertised: Speeds { g10: true, g1: true, m100: false }, restarted: true,
    });
    // LINKS up but copper down: not up.
    let mut io = Fake::ready(1);
    x557(&mut io, 3, false);
    io.regs.insert(LINKS, UP_10G);
    let w = hardware::wait_link(&mut io, p, Some(&setup), 500).unwrap();
    assert_eq!((w.link, w.copper), (Link::Down, Some(Copper::Down)));
    // Copper up at 1G: iXFI re-forced to 1G once.
    io.mdio.insert((3, 7, 1), 1 << 2);
    io.mdio.insert((3, 7, 0xc800), 5);
    io.regs.insert(LINKS, (1 << 30) | (2 << 28));
    let w = hardware::wait_link(&mut io, p, Some(&setup), 500).unwrap();
    assert_eq!((w.link, w.copper, w.reforced), (Link::Up { megabits: Some(1000) }, Some(Copper::Up { megabits: 1000 }), 1));
    assert_eq!(io.kr[&0x820c] & (7 << 8), 2 << 8);
    // Copper at 100M: not a speed the internal link carries.
    io.mdio.insert((3, 7, 0xc800), 3);
    let w = hardware::wait_link(&mut io, p, Some(&setup), 0).unwrap();
    assert_eq!((w.link, w.copper), (Link::Down, Some(Copper::Invalid { status: 3 })));
    // The watch re-forces once per link-up or speed change, not per poll.
    let mut watch = x552::Watch::new(3, Internal::Ixfi);
    io.mdio.insert((3, 7, 0xc800), 7);
    assert_eq!(watch.poll(&mut io, p), Ok(Copper::Up { megabits: 10_000 }));
    let n = io.kr_writes.len();
    assert_eq!(watch.poll(&mut io, p), Ok(Copper::Up { megabits: 10_000 }));
    assert_eq!(io.kr_writes.len(), n);
    assert_eq!(watch.reforced, 1);
}
#[test]
fn x552_10gbase_t_kr_mode_sets_kr_at_copper_link_up_only() {
    let mut io = Fake::ready(0);
    x557(&mut io, 5, false);
    io.regs.insert(0x11178, (1 << 24) | (5 << 3));
    let (prepared, _) = run(&mut io, Family::X552, 0x15ad);
    assert!(matches!(prepared, Prepared::X552(x552::Prepared::Copper { phy: 5, unstalled: false, internal: Internal::Kr, .. })));
    assert!(io.kr_writes.is_empty(), "no internal link set-up before copper is up");
    assert!(!io.ops.iter().any(|op| matches!(op, Op::Write(MSCA, v) if (v >> 21) & 0x1f != 5)), "only NW_MNG_IF_SEL's address");
    let mut watch = x552::Watch::new(5, Internal::Kr);
    io.mdio.insert((5, 7, 1), 1 << 2);
    io.mdio.insert((5, 7, 0xc800), 7);
    watch.poll(&mut io, port(Family::X552, 0x15ad, 0)).unwrap();
    assert_eq!(io.kr[&0x420c], LC1_AN_ENABLE | LC1_KR | LC1_KX | LC1_AN_RESTART);
}
#[test]
fn x552_10gbase_t_without_a_phy_fails_and_veto_skips_the_phy_reset() {
    let mut io = Fake::ready(0);
    assert_eq!(hardware::prepare(&mut io, port(Family::X552, 0x15ad, 0), false), Err(Error::NoPhy));
    let mut io = Fake::ready(0);
    x557(&mut io, 0, false);
    let p = hardware::prepare(&mut io, port(Family::X552, 0x15ad, 0), true).unwrap();
    assert!(matches!(p, Prepared::X552(x552::Prepared::Copper { reset: false, .. })));
    assert!(io.mdio_writes.is_empty());
}
#[test]
fn x552_sfp_resets_cs4227_once_and_sets_passive_da_multispeed_on_port_1() {
    let mut io = Fake::ready(1);
    io.i2c = Slave::cs4227(&[]);
    io.i2c.sfp = sfp(&[(8, 0x04)]);
    io.regs.insert(ESDP, (1 << 17) | (1 << 16) | (1 << 8) | 2);
    io.regs.insert(LINKS, UP_10G);
    let (prepared, setup) = run(&mut io, Family::X552, 0x15ac);
    assert!(matches!(prepared, Prepared::X552(x552::Prepared::Sfp { cs4227_reset: true, module }) if module.kind == Kind::DaCu && module.multispeed));
    let Ok(Setup::X552(x552::Setup::Sfp { link_ctrl, edc, speed, rate_select, .. })) = setup else { panic!("{setup:?}") };
    assert_eq!((link_ctrl, edc, speed, rate_select), (Some(LC1_AN_ENABLE | LC1_KR), Some(2), Some(10_000), true));
    assert_eq!(io.i2c.cs_resets, 1);
    assert_eq!(io.i2c.cs[&2], 0x5aa5, "reset recorded for the other port");
    assert_eq!(io.i2c.cs[&0x22b0], 0x0005, "port 1 line side: CX1 EDC");
    assert_eq!(io.kr[&0x820c], LC1_AN_ENABLE | LC1_KR | LC1_AN_RESTART);
    assert_eq!((io.i2c.diag[0x6e], io.i2c.diag[0x76]), (8, 8), "soft rate select 10G");
    // Mux control: SDP0/SDP1 GPIO, SDP1 output; mux selected while held, released after.
    assert_eq!(io.reg(ESDP), 1 << 9);
    assert!(io.ops.contains(&Op::Write(ESDP, (1 << 9) | 2)));
    // The CS4227's checksum byte is NACKed: every combined read ends after 3 bytes.
    assert!(io.i2c.nacks.iter().filter(|n| n.0 == 0xbe).all(|n| n.1 == 3));
    assert!(io.i2c.nacks.iter().any(|n| n.0 == 0xbe));
    released(&io);
    assert_eq!(io.i2c.phase, Phase::Idle);

    // Second start (the other port already reset it): no second reset;
    // 10G SR is single speed, SR EDC, no rate select.
    let mut io = Fake::ready(0);
    io.i2c = Slave::cs4227(&[(2, 0x5aa5)]);
    io.i2c.sfp = sfp(&[(3, 0x10)]);
    let (_, setup) = run(&mut io, Family::X552, 0x15ac);
    let Ok(Setup::X552(x552::Setup::Sfp { cs4227_reset, edc, speed, .. })) = setup else { panic!() };
    assert_eq!((cs4227_reset, edc, speed), (false, Some(4), None));
    assert_eq!(io.i2c.cs_resets, 0);
    assert_eq!(io.i2c.cs[&0x12b0], 0x0009, "port 0 line side: SR EDC");
    assert_eq!(io.i2c.diag[0x6e], 0);
}
#[test]
fn x552_sfp_multispeed_falls_back_to_1g_then_back_to_10g() {
    let mut io = Fake::ready(0);
    io.i2c = Slave::cs4227(&[(2, 0x5aa5)]);
    io.i2c.sfp = sfp(&[(3, 0x10), (6, 0x01)]); // SR + SX: multispeed optics
    let (_, setup) = run(&mut io, Family::X552, 0x15ac);
    let Ok(Setup::X552(x552::Setup::Sfp { link_ctrl, speed, .. })) = setup else { panic!() };
    // (The fake KR PHY keeps AN_RESTART set; whether it self-clears is spec 10 item 7.)
    assert_eq!((link_ctrl.map(|v| v & !LC1_AN_RESTART), speed), (Some(LC1_AN_ENABLE | LC1_KR), None), "left at 10G, waiting");
    let caps: Vec<u32> = io.kr_writes.iter().step_by(2).map(|w| w.1 & (LC1_KR | LC1_KX)).collect();
    assert_eq!(caps, [LC1_KR, LC1_KX, LC1_KR]);
    assert_eq!(io.i2c.cs[&0x12b0], 0x0009);
}
#[test]
fn x552_sfp_1g_optics_advertise_kx_only() {
    let mut io = Fake::ready(0);
    io.i2c = Slave::cs4227(&[(2, 0x5aa5)]);
    io.i2c.sfp = sfp(&[(6, 0x02)]);
    let (_, setup) = run(&mut io, Family::X552, 0x15ac);
    let Ok(Setup::X552(x552::Setup::Sfp { module, link_ctrl, .. })) = setup else { panic!() };
    assert_eq!((module.kind, link_ctrl), (Kind::Lx1g, Some(LC1_AN_ENABLE | LC1_KX)));
}
#[test]
fn sfp_classification_follows_the_spec_order() {
    for (bytes, kind) in [
        (vec![(6, 0x40), (12, 0x67), (14, 1)], Kind::Bx10g),
        (vec![(6, 0x40), (12, 0x67), (15, 5)], Kind::Bx1g),
        (vec![(6, 0x08)], Kind::Cu1g),
        (vec![(8, 0x08), (60, 0x04)], Kind::DaActiveLimiting),
        (vec![(0, 0x0d)], Kind::Unknown),
    ] {
        let mut io = Fake::ready(0);
        io.i2c = Slave::cs4227(&[(2, 0x5aa5)]);
        io.i2c.sfp = sfp(&bytes);
        let (prepared, setup) = run(&mut io, Family::X552, 0x15ac);
        let Prepared::X552(x552::Prepared::Sfp { module, .. }) = prepared else { panic!() };
        assert_eq!(module.kind, kind, "{bytes:?}");
        let supported = !matches!(kind, Kind::Cu1g | Kind::Unknown);
        assert!(matches!(setup, Ok(Setup::X552(x552::Setup::Sfp { link_ctrl, .. })) if link_ctrl.is_some() == supported));
        released(&io);
    }
}
#[test]
fn x552_sfp_absent_module_is_not_set_up() {
    let mut io = Fake::ready(0);
    io.i2c = Slave::cs4227(&[(2, 0x5aa5)]);
    let (prepared, setup) = run(&mut io, Family::X552, 0x15ac);
    assert!(matches!(prepared, Prepared::X552(x552::Prepared::Sfp { module, .. }) if module.kind == Kind::NotPresent));
    assert!(matches!(setup, Ok(Setup::X552(x552::Setup::Sfp { link_ctrl: None, edc: None, .. }))));
    assert!(io.kr_writes.is_empty());
    assert!(!io.i2c.cs.contains_key(&0x12b0));
    released(&io);
}
#[test]
fn x552_cs4227_that_never_loads_fails_and_releases() {
    let mut io = Fake::ready(0);
    io.i2c = Slave::cs4227(&[(0x181, 0)]);
    io.i2c.sfp = sfp(&[(3, 0x10)]);
    let lan = hardware::begin(&mut io).unwrap();
    assert_eq!(hardware::prepare(&mut io, port(Family::X552, 0x15ac, lan), false),
        Err(Error::Cs4227 { register: 0x181, value: 0 }));
    assert!(io.kr_writes.is_empty());
    released(&io);
}
#[test]
fn x552_cs4227_waits_for_a_pending_peer_then_takes_over() {
    let mut io = Fake::ready(0);
    io.i2c = Slave::cs4227(&[(2, 0x1357)]);
    io.i2c.sfp = sfp(&[(3, 0x10)]);
    let lan = hardware::begin(&mut io).unwrap();
    let p = hardware::prepare(&mut io, port(Family::X552, 0x15ac, lan), false).unwrap();
    assert!(matches!(p, Prepared::X552(x552::Prepared::Sfp { cs4227_reset: true, .. })));
    assert!(io.delays(30_000) >= 15);
    assert_eq!(io.i2c.cs[&2], 0x5aa5);
}
#[test]
fn x552_sfp_reads_the_crosstalk_word_through_the_host_interface() {
    let sfp_port = || {
        let mut io = Fake::ready(0);
        io.i2c = Slave::cs4227(&[(2, 0x5aa5)]);
        io.i2c.sfp = sfp(&[(3, 0x10)]);
        io.regs.insert(LINKS, UP_10G);
        io
    };
    let mut io = sfp_port();
    io.regs.insert(HICR, 1);
    io.nvm.insert(0x2c, 0x0001);
    let (_, setup) = run(&mut io, Family::X552, 0x15ac);
    let setup = setup.unwrap();
    assert!(matches!(setup, Setup::X552(x552::Setup::Sfp { crosstalk: Some(true), .. })), "{setup:?}");
    assert_eq!(io.writes_to(FLEX_MNG + 4), [0x5800_0000], "byte address 0x58, big-endian");
    assert!(io.ops.contains(&Op::Write(SWFW_SYNC, 0x401)), "SW_MNG and EEP held");
    assert_ne!(io.reg(0x15f0c) & (1 << 9), 0, "FWSTS.FWRI cleared (write 1)");
    released(&io);
    // SDP0 is the X552 cage-presence pin.
    let p = port(Family::X552, 0x15ac, 0);
    assert_eq!(hardware::wait_link(&mut io, p, Some(&setup), 0).unwrap().link, Link::Down);
    let esdp = io.reg(ESDP);
    io.regs.insert(ESDP, esdp | 1);
    assert_eq!(hardware::wait_link(&mut io, p, Some(&setup), 0).unwrap().link, Link::Up { megabits: Some(10_000) });
    // Bit 7 set: no fix.
    let mut io = sfp_port();
    io.regs.insert(HICR, 1);
    io.nvm.insert(0x2c, 0x0080);
    let (_, setup) = run(&mut io, Family::X552, 0x15ac);
    assert!(matches!(setup, Ok(Setup::X552(x552::Setup::Sfp { crosstalk: Some(false), .. }))));
    // Host interface disabled, or a command with no valid status: unknown, fix off, not fatal.
    let mut io = sfp_port();
    let (_, setup) = run(&mut io, Family::X552, 0x15ac);
    assert!(matches!(setup, Ok(Setup::X552(x552::Setup::Sfp { crosstalk: None, .. }))));
    released(&io);
    let mut io = sfp_port();
    io.regs.insert(HICR, 1);
    io.stuck = Some(HICR);
    let p = port(Family::X552, 0x15ac, 0);
    assert_eq!(x552::nvm_word(&mut io, p, 0x2c), Err(Error::HostInterface { hicr: 1 }));
    released(&io);
}
