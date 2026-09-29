//! A simulated NIC that DMAs, shared by `test/rings.rs` and `test/snp.rs`:
//! it fetches TX descriptors and buffers from the region on a TDT write,
//! writes back DD, and delivers frames (looped back or injected) into the RX
//! ring the way the datasheet's legacy write-back describes, through the
//! receive filters (FCTRL, RAR0, MTA with MCSTCTRL.MFE).
use crate::hardware::rings::{Dma, Filter, Rings, BUF_SIZE, DMA_BYTES};
use crate::hardware::Registers;
use std::collections::BTreeMap;

pub const CTRL: u32 = 0x0;
pub const RDBAL: u32 = 0x1000;
pub const RDBAH: u32 = 0x1004;
pub const RDLEN: u32 = 0x1008;
pub const RDH: u32 = 0x1010;
pub const SRRCTL: u32 = 0x1014;
pub const RDT: u32 = 0x1018;
pub const RXDCTL: u32 = 0x1028;
pub const TDBAL: u32 = 0x6000;
pub const TDBAH: u32 = 0x6004;
pub const TDLEN: u32 = 0x6008;
pub const TDH: u32 = 0x6010;
pub const TDT: u32 = 0x6018;
pub const TXDCTL: u32 = 0x6028;
pub const RDRXCTL: u32 = 0x2f00;
pub const RXCTRL: u32 = 0x3000;
pub const HLREG0: u32 = 0x4240;
pub const DMATXCTL: u32 = 0x4a80;
pub const FCTRL: u32 = 0x5080;
pub const MTA: u32 = 0x5200;
pub const MCSTCTRL: u32 = 0x5090;
pub const RAL0: u32 = 0xa200;
pub const RAH0: u32 = 0xa204;
pub const LINKS: u32 = 0x42a4;
pub const GPRC: u32 = 0x4074;
pub const GPTC: u32 = 0x4080;
pub const ENABLE: u32 = 1 << 25;
/// Above 4 GB, so the high halves of the base registers matter.
pub const DEVICE: u64 = 0x1_2345_0000;
pub const MAC: [u8; 6] = [0x02, 0x11, 0x22, 0x33, 0x44, 0x55];

pub struct Nic {
    pub regs: BTreeMap<u32, u32>,
    pub ops: Vec<(u32, u32)>,
    pub mem: *mut u8,
    /// Transmitted frames go straight back into receive (MAC loopback).
    pub loopback: bool,
    /// TX is not fetched (no link): descriptors stay pending.
    pub hold_tx: bool,
    /// Register whose ENABLE bit never reads back set.
    pub stuck: Option<u32>,
    /// Frames that arrive at the next delay.
    pub inbound: Vec<Vec<u8>>,
    pub wire: Vec<Vec<u8>>,
    pub dropped: usize,
    pub delays: usize,
}

impl Nic {
    pub fn new(mem: &mut Vec<u8>) -> Self {
        Nic {
            regs: BTreeMap::from([(CTRL, 1 << 2), (HLREG0, 1 << 0 | 1 << 10), (RDRXCTL, 1 << 3), (0x5088, 1 << 30),
                // RAR0 as the NVM loads it (AV set); LINKS up at 10G.
                (RAL0, u32::from_le_bytes([MAC[0], MAC[1], MAC[2], MAC[3]])),
                (RAH0, u32::from(u16::from_le_bytes([MAC[4], MAC[5]])) | 1 << 31),
                (LINKS, 1 << 30 | 3 << 28)]),
            ops: vec![], mem: mem.as_mut_ptr(), loopback: false, hold_tx: false, stuck: None,
            inbound: vec![], wire: vec![], dropped: 0, delays: 0,
        }
    }
    pub fn reg(&self, r: u32) -> u32 { *self.regs.get(&r).unwrap_or(&0) }
    pub fn at(&self, device: u64, len: usize) -> *mut u8 {
        assert!(device >= DEVICE && device + len as u64 <= DEVICE + DMA_BYTES as u64, "DMA outside the region: {device:#x}");
        unsafe { self.mem.add((device - DEVICE) as usize) }
    }
    pub fn base(&self, lo: u32, hi: u32) -> u64 { self.reg(lo) as u64 | (self.reg(hi) as u64) << 32 }
    pub fn bump(&mut self, r: u32) { let v = self.reg(r); self.regs.insert(r, v + 1); }

    pub fn process_tx(&mut self) {
        if self.hold_tx || self.reg(TXDCTL) & ENABLE == 0 || self.reg(DMATXCTL) & 1 == 0 { return; }
        let n = self.reg(TDLEN) as usize / 16;
        let ring = self.base(TDBAL, TDBAH);
        while self.reg(TDH) != self.reg(TDT) {
            let h = self.reg(TDH) as usize;
            let d = self.at(ring + 16 * h as u64, 16);
            let frame = unsafe {
                let addr = (d as *const u64).read_unaligned();
                let len = (d.add(8) as *const u16).read_unaligned() as usize;
                assert_eq!(*d.add(11), 0x0b, "legacy CMD: EOP|IFCS|RS, DEXT clear");
                std::slice::from_raw_parts(self.at(addr, len), len).to_vec()
            };
            unsafe { *d.add(12) |= 1; }
            self.regs.insert(TDH, ((h + 1) % n) as u32);
            self.bump(GPTC);
            if self.loopback { self.deliver(&frame); }
            self.wire.push(frame);
        }
    }

    /// Receive one frame into the ring, or drop it (filter, RX off, ring full).
    pub fn deliver(&mut self, frame: &[u8]) {
        let f = self.reg(FCTRL);
        let d = &frame[..6];
        let broadcast = d == [0xff; 6];
        let station = self.reg(RAH0) & 1 << 31 != 0 && d[..4] == self.reg(RAL0).to_le_bytes()
            && d[4..] == (self.reg(RAH0) as u16).to_le_bytes();
        // MTA, MO = 00: address bits 47:36 (byte 5, high nibble of byte 4).
        let hash = (d[5] as u32) << 4 | (d[4] as u32) >> 4;
        let mta = self.reg(MCSTCTRL) & 1 << 2 != 0 && self.reg(MTA + 4 * (hash >> 5)) & 1 << (hash & 31) != 0;
        let multicast = !broadcast && d[0] & 1 != 0 && (f & (1 << 8) != 0 || mta);
        let passes = (broadcast && f & (1 << 10) != 0) || station || multicast || f & (1 << 9) != 0;
        let n = self.reg(RDLEN) as usize / 16;
        if !passes || self.reg(RXCTRL) & 1 == 0 || self.reg(RXDCTL) & ENABLE == 0 || self.reg(RDH) == self.reg(RDT) {
            self.dropped += 1;
            return;
        }
        let h = self.reg(RDH) as usize;
        let d = self.at(self.base(RDBAL, RDBAH) + 16 * h as u64, 16);
        unsafe {
            let addr = (d as *const u64).read_unaligned();
            assert!(*d.add(12) & 1 == 0, "NIC given a descriptor still holding a frame");
            std::ptr::copy_nonoverlapping(frame.as_ptr(), self.at(addr, BUF_SIZE), frame.len());
            (d.add(8) as *mut u16).write_unaligned(frame.len() as u16);
            *d.add(12) = 0b11; // DD | EOP
            *d.add(13) = 0;
        }
        self.regs.insert(RDH, ((h + 1) % n) as u32);
        self.bump(GPRC);
    }
}

impl Registers for Nic {
    type Error = &'static str;
    fn read(&mut self, r: u32) -> Result<u32, Self::Error> {
        let v = self.reg(r);
        if r == GPRC || r == GPTC { self.regs.insert(r, 0); }
        Ok(v)
    }
    fn write(&mut self, r: u32, v: u32) -> Result<(), Self::Error> {
        self.ops.push((r, v));
        let v = if self.stuck == Some(r) { v & !ENABLE } else { v };
        self.regs.insert(r, v);
        if r == TDT { self.process_tx(); }
        Ok(())
    }
    fn delay_us(&mut self, _: usize) {
        self.delays += 1;
        for f in std::mem::take(&mut self.inbound) { self.deliver(&f); }
    }
}

pub fn setup() -> (Vec<u8>, Nic, Rings) {
    let mut mem = vec![0xa5u8; DMA_BYTES];
    let nic = Nic::new(&mut mem);
    let rings = unsafe { Rings::new(Dma { host: mem.as_mut_ptr(), device: DEVICE }) };
    (mem, nic, rings)
}
pub fn started() -> (Vec<u8>, Nic, Rings) {
    let (mem, mut nic, mut rings) = setup();
    rings.start(&mut nic, Filter { broadcast: true, ..Filter::default() }).unwrap();
    (mem, nic, rings)
}
pub fn frame(dst: [u8; 6], seq: u16, len: usize) -> Vec<u8> {
    let mut f = vec![0u8; len];
    f[..6].copy_from_slice(&dst);
    f[6..12].copy_from_slice(&[0x02, 0, 0, 0, 0, 9]);
    f[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
    f[14..16].copy_from_slice(&seq.to_be_bytes());
    f
}
pub fn pos(nic: &Nic, op: (u32, u32)) -> usize {
    nic.ops.iter().position(|o| *o == op).unwrap_or_else(|| panic!("no write {:#x} = {:#x}", op.0, op.1))
}
