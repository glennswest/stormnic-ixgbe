//! Descriptor rings (#3) against a simulated NIC that DMAs: it fetches TX
//! descriptors and buffers from the region on a TDT write, writes back DD,
//! and delivers frames (looped back or injected) into the RX ring the way
//! the datasheet's legacy write-back describes.
#[path = "../src/hardware.rs"]
#[allow(dead_code)]
mod hardware;
use hardware::rings::{self, Dma, Filter, Rings, BUF_SIZE, DMA_BYTES, RX_DESCS, TX_DESCS};
use hardware::{Error, Registers};
use std::collections::BTreeMap;

const CTRL: u32 = 0x0;
const RDBAL: u32 = 0x1000;
const RDBAH: u32 = 0x1004;
const RDLEN: u32 = 0x1008;
const RDH: u32 = 0x1010;
const SRRCTL: u32 = 0x1014;
const RDT: u32 = 0x1018;
const RXDCTL: u32 = 0x1028;
const TDBAL: u32 = 0x6000;
const TDBAH: u32 = 0x6004;
const TDLEN: u32 = 0x6008;
const TDH: u32 = 0x6010;
const TDT: u32 = 0x6018;
const TXDCTL: u32 = 0x6028;
const RDRXCTL: u32 = 0x2f00;
const RXCTRL: u32 = 0x3000;
const HLREG0: u32 = 0x4240;
const DMATXCTL: u32 = 0x4a80;
const FCTRL: u32 = 0x5080;
const MTA: u32 = 0x5200;
const GPRC: u32 = 0x4074;
const GPTC: u32 = 0x4080;
const ENABLE: u32 = 1 << 25;
/// Above 4 GB, so the high halves of the base registers matter.
const DEVICE: u64 = 0x1_2345_0000;
const MAC: [u8; 6] = [0x02, 0x11, 0x22, 0x33, 0x44, 0x55];

struct Nic {
    regs: BTreeMap<u32, u32>,
    ops: Vec<(u32, u32)>,
    mem: *mut u8,
    /// Transmitted frames go straight back into receive (MAC loopback).
    loopback: bool,
    /// TX is not fetched (no link): descriptors stay pending.
    hold_tx: bool,
    /// Register whose ENABLE bit never reads back set.
    stuck: Option<u32>,
    /// Frames that arrive at the next delay.
    inbound: Vec<Vec<u8>>,
    wire: Vec<Vec<u8>>,
    dropped: usize,
    delays: usize,
}

impl Nic {
    fn new(mem: &mut Vec<u8>) -> Self {
        Nic {
            regs: BTreeMap::from([(CTRL, 1 << 2), (HLREG0, 1 << 0 | 1 << 10), (RDRXCTL, 1 << 3), (0x5088, 1 << 30)]),
            ops: vec![], mem: mem.as_mut_ptr(), loopback: false, hold_tx: false, stuck: None,
            inbound: vec![], wire: vec![], dropped: 0, delays: 0,
        }
    }
    fn reg(&self, r: u32) -> u32 { *self.regs.get(&r).unwrap_or(&0) }
    fn at(&self, device: u64, len: usize) -> *mut u8 {
        assert!(device >= DEVICE && device + len as u64 <= DEVICE + DMA_BYTES as u64, "DMA outside the region: {device:#x}");
        unsafe { self.mem.add((device - DEVICE) as usize) }
    }
    fn base(&self, lo: u32, hi: u32) -> u64 { self.reg(lo) as u64 | (self.reg(hi) as u64) << 32 }
    fn bump(&mut self, r: u32) { let v = self.reg(r); self.regs.insert(r, v + 1); }

    fn process_tx(&mut self) {
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
    fn deliver(&mut self, frame: &[u8]) {
        let f = self.reg(FCTRL);
        let broadcast = frame[..6] == [0xff; 6];
        let passes = (broadcast && f & (1 << 10) != 0) || frame[..6] == MAC || f & (1 << 9) != 0;
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

fn setup() -> (Vec<u8>, Nic, Rings) {
    let mut mem = vec![0xa5u8; DMA_BYTES];
    let nic = Nic::new(&mut mem);
    let rings = unsafe { Rings::new(Dma { host: mem.as_mut_ptr(), device: DEVICE }) };
    (mem, nic, rings)
}
fn started() -> (Vec<u8>, Nic, Rings) {
    let (mem, mut nic, mut rings) = setup();
    rings.start(&mut nic, Filter { broadcast: true, ..Filter::default() }).unwrap();
    (mem, nic, rings)
}
fn frame(dst: [u8; 6], seq: u16, len: usize) -> Vec<u8> {
    let mut f = vec![0u8; len];
    f[..6].copy_from_slice(&dst);
    f[6..12].copy_from_slice(&[0x02, 0, 0, 0, 0, 9]);
    f[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
    f[14..16].copy_from_slice(&seq.to_be_bytes());
    f
}
fn pos(nic: &Nic, op: (u32, u32)) -> usize {
    nic.ops.iter().position(|o| *o == op).unwrap_or_else(|| panic!("no write {:#x} = {:#x}", op.0, op.1))
}

#[test]
fn start_programs_queue_0_from_the_datasheet_sequence() {
    let (_mem, nic, _rings) = started();
    assert_eq!(nic.reg(CTRL) & (1 << 2), 0, "GIO master disable cleared");
    assert_eq!(nic.ops.iter().filter(|(r, v)| (MTA..MTA + 512).contains(r) && *v == 0).count(), 128);
    assert_eq!(nic.reg(0x5088) & (1 << 30), 0, "VLAN filter off");
    assert_eq!(nic.reg(FCTRL), 1 << 10, "broadcast accepted, not promiscuous");
    assert_eq!(nic.reg(HLREG0), 1 << 0 | 1 << 1 | 1 << 10, "RX CRC strip, TX CRC and pad kept");
    assert_eq!(nic.reg(RDRXCTL), 1 << 3 | 1 << 1, "CRCSTRIP matches HLREG0");
    assert_eq!((nic.reg(RDBAL), nic.reg(RDBAH), nic.reg(RDLEN)), (0x2345_0000, 1, 512));
    assert_eq!(nic.reg(SRRCTL), 1 << 28 | 2, "2 KB buffers, legacy, drop when full");
    assert_eq!((nic.reg(RDH), nic.reg(RDT)), (0, RX_DESCS as u32 - 1));
    assert_eq!((nic.reg(TDBAL), nic.reg(TDBAH), nic.reg(TDLEN)), (0x2345_0200, 1, 512));
    assert_eq!((nic.reg(TDH), nic.reg(TDT)), (0, 0));
    assert_eq!(nic.reg(TXDCTL), ENABLE, "thresholds 0: write-back per RS");
    // Order: RX queue enabled, then RDT bumped, then RXEN; TE before TX queue.
    let rx_on = pos(&nic, (RXDCTL, ENABLE));
    let tail = pos(&nic, (RDT, RX_DESCS as u32 - 1));
    let rxen = pos(&nic, (RXCTRL, 1));
    assert!(pos(&nic, (RDLEN, 512)) < rx_on && rx_on < tail && tail < rxen);
    assert!(pos(&nic, (DMATXCTL, 1)) < pos(&nic, (TXDCTL, ENABLE)));
}

#[test]
fn broadcast_frame_round_trips_through_tx_and_rx_rings() {
    let (_mem, mut nic, mut rings) = started();
    nic.loopback = true;
    let c = rings::check(&mut nic, &mut rings, MAC, 3000).unwrap();
    assert!(c.sent);
    assert_eq!((c.received, c.waited_ms, c.gptc, c.gprc), (1, 0, 1, 1));
    let f = c.first.unwrap();
    assert_eq!((f.len, f.destination, f.source, f.ethertype), (60, [0xff; 6], MAC, 0x88b5));
    assert_eq!(nic.wire, vec![rings::check_frame(MAC).to_vec()]);
    assert_eq!(&nic.wire[0][14..38], b"stormnic-ixgbe DMA check");
    // The RX descriptor went back to the NIC.
    assert_eq!((nic.reg(RDH), nic.reg(RDT)), (1, 0));
    assert_eq!(rings.receive(&mut nic, &mut [0; BUF_SIZE]).unwrap(), None);
}

#[test]
fn check_on_a_quiet_network_sends_and_times_out_listening() {
    let (_mem, mut nic, mut rings) = started();
    let c = rings::check(&mut nic, &mut rings, MAC, 50).unwrap();
    assert!(c.sent);
    assert_eq!((c.received, c.first, c.waited_ms, c.gptc, c.gprc), (0, None, 50, 1, 0));
    assert_eq!(nic.delays, 50);
}

#[test]
fn check_reports_the_first_frame_from_the_network() {
    let (_mem, mut nic, mut rings) = started();
    nic.inbound = vec![frame([0xff; 6], 1, 60), frame(MAC, 2, 342), frame([0x02, 9, 9, 9, 9, 9], 3, 60)];
    let c = rings::check(&mut nic, &mut rings, MAC, 3000).unwrap();
    // The frame for another station is filtered; the other two arrive.
    assert_eq!((c.received, c.gprc, nic.dropped), (2, 2, 1));
    assert_eq!(c.first.unwrap().destination, [0xff; 6]);
}

#[test]
fn unsent_frame_is_reported_not_waited_on_forever() {
    let (_mem, mut nic, mut rings) = started();
    nic.hold_tx = true;
    let c = rings::check(&mut nic, &mut rings, MAC, 0).unwrap();
    assert!(!c.sent);
    assert_eq!(nic.delays, 101);
}

#[test]
fn rx_ring_wraps_and_every_frame_is_received_in_order() {
    let (_mem, mut nic, mut rings) = started();
    let mut buf = [0u8; BUF_SIZE];
    let mut seq = 0u16;
    for batch in 0..10 {
        for _ in 0..(7 + batch) {
            nic.deliver(&frame(MAC, seq, 64 + seq as usize));
            seq += 1;
        }
        let mut got = 0;
        while let Some(len) = rings.receive(&mut nic, &mut buf).unwrap() {
            let s = u16::from_be_bytes([buf[14], buf[15]]);
            assert_eq!(len, 64 + s as usize);
            got += 1;
        }
        assert_eq!(got, 7 + batch);
    }
    assert_eq!((seq, nic.dropped), (115, 0));
}

#[test]
fn full_rx_ring_drops_at_the_nic_and_recovers() {
    let (_mem, mut nic, mut rings) = started();
    for s in 0..40 { nic.deliver(&frame(MAC, s, 60)); }
    // One descriptor is always kept back (RDH == RDT means empty).
    assert_eq!(nic.dropped, 40 - (RX_DESCS - 1));
    let mut buf = [0u8; BUF_SIZE];
    let mut n = 0;
    while rings.receive(&mut nic, &mut buf).unwrap().is_some() { n += 1; }
    assert_eq!(n, RX_DESCS - 1);
    nic.deliver(&frame(MAC, 99, 60));
    assert_eq!(rings.receive(&mut nic, &mut buf).unwrap(), Some(60));
    assert_eq!(u16::from_be_bytes([buf[14], buf[15]]), 99);
}

#[test]
fn errored_and_multi_buffer_frames_are_dropped() {
    let (_mem, mut nic, mut rings) = started();
    for s in 0..3 { nic.deliver(&frame(MAC, s, 60)); }
    let base = nic.mem;
    let d = |i: usize| unsafe { base.add(16 * i) };
    unsafe {
        *d(0).add(13) = 1; // CE: CRC error
        *d(1).add(12) = 1; // DD without EOP
    }
    let mut buf = [0u8; BUF_SIZE];
    assert_eq!(rings.receive(&mut nic, &mut buf).unwrap(), Some(60));
    assert_eq!(u16::from_be_bytes([buf[14], buf[15]]), 2);
    // Both dropped descriptors were handed back.
    assert_eq!(nic.reg(RDT), 2);
}

#[test]
fn frame_lengths_are_checked_and_a_short_buffer_keeps_the_frame() {
    let (_mem, mut nic, mut rings) = started();
    assert_eq!(rings.transmit(&mut nic, &[0; 13]), Err(Error::FrameLength { len: 13 }));
    assert_eq!(rings.transmit(&mut nic, &[0; 1519]), Err(Error::FrameLength { len: 1519 }));
    assert_eq!(rings.transmit(&mut nic, &frame([0xff; 6], 0, 1518)), Ok(true));
    nic.deliver(&frame(MAC, 7, 1514));
    let mut small = [0u8; 100];
    assert_eq!(rings.receive(&mut nic, &mut small), Err(Error::FrameLength { len: 1514 }));
    let mut buf = [0u8; BUF_SIZE];
    assert_eq!(rings.receive(&mut nic, &mut buf).unwrap(), Some(1514));
    assert_eq!(u16::from_be_bytes([buf[14], buf[15]]), 7);
}

#[test]
fn tx_ring_fills_then_reclaims_what_the_nic_sent() {
    let (_mem, mut nic, mut rings) = started();
    nic.hold_tx = true;
    for s in 0..(TX_DESCS - 1) as u16 {
        assert_eq!(rings.transmit(&mut nic, &frame([0xff; 6], s, 60)), Ok(true));
    }
    assert_eq!(rings.transmit(&mut nic, &frame([0xff; 6], 99, 60)), Ok(false), "ring full");
    assert_eq!(rings.tx_pending(), TX_DESCS - 1);
    nic.hold_tx = false;
    nic.process_tx();
    assert_eq!(rings.reclaim(), TX_DESCS - 1);
    // Wraps: 40 more, each sent at once.
    for s in 0..40u16 { assert_eq!(rings.transmit(&mut nic, &frame([0xff; 6], 100 + s, 60)), Ok(true)); }
    assert_eq!(rings.tx_pending(), 0);
    let seqs: Vec<u16> = nic.wire.iter().map(|f| u16::from_be_bytes([f[14], f[15]])).collect();
    let want: Vec<u16> = (0..31).chain(100..140).collect();
    assert_eq!(seqs, want);
}

#[test]
fn stop_drains_briefly_then_disables_tx_rx_and_mastering() {
    let (_mem, mut nic, mut rings) = started();
    nic.hold_tx = true;
    rings.transmit(&mut nic, &frame([0xff; 6], 0, 60)).unwrap();
    assert_eq!(rings.stop(&mut nic), Ok(1), "the unsent frame is reported");
    assert_eq!(nic.delays, 100);
    assert_eq!(nic.reg(TXDCTL) & ENABLE, 0);
    assert_eq!(nic.reg(DMATXCTL) & 1, 0);
    assert_eq!(nic.reg(RXDCTL) & ENABLE, 0);
    assert_eq!(nic.reg(RXCTRL) & 1, 0);
    assert_ne!(nic.reg(CTRL) & (1 << 2), 0, "GIO master disable set");
    // Restartable (the SNP will start and stop the rings again).
    nic.hold_tx = false;
    nic.loopback = true;
    rings.start(&mut nic, Filter { broadcast: true, ..Filter::default() }).unwrap();
    assert!(rings::check(&mut nic, &mut rings, MAC, 10).unwrap().received == 1);
}

#[test]
fn queue_that_never_enables_times_out() {
    let (_mem, mut nic, mut rings) = setup();
    nic.stuck = Some(RXDCTL);
    let e = rings.start(&mut nic, Filter::default()).unwrap_err();
    assert!(matches!(e, Error::Timeout { register: RXDCTL, .. }), "{e:?}");
    let (_mem, mut nic, mut rings) = setup();
    nic.stuck = Some(TXDCTL);
    let e = rings.start(&mut nic, Filter::default()).unwrap_err();
    assert!(matches!(e, Error::Timeout { register: TXDCTL, .. }), "{e:?}");
}

#[test]
fn broadcast_is_filtered_unless_enabled_and_filters_are_independent() {
    let (_mem, mut nic, mut rings) = setup();
    rings.start(&mut nic, Filter::default()).unwrap();
    nic.loopback = true;
    let c = rings::check(&mut nic, &mut rings, MAC, 5).unwrap();
    assert_eq!((c.sent, c.received), (true, 0));
    rings.set_filter(&mut nic, Filter { broadcast: true, all_multicast: true, promiscuous: true }).unwrap();
    assert_eq!(nic.reg(FCTRL), 1 << 10 | 1 << 8 | 1 << 9);
    rings.set_filter(&mut nic, Filter { all_multicast: true, ..Filter::default() }).unwrap();
    assert_eq!(nic.reg(FCTRL), 1 << 8);
}
