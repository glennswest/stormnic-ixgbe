//! Receive and transmit descriptor rings (#3), from the 82599 datasheet's
//! receive and transmit initialization sections and its legacy descriptor
//! formats; the X540 and X552 use the same registers and layouts for queue 0.
//!
//! Independent of UEFI, like the rest of `hardware`: the caller hands in one
//! DMA region (host pointer and the device address the NIC uses for it,
//! mapped as a common buffer) and firmware Stall through `Registers`.
//!
//! One RX and one TX queue (queue 0), legacy descriptors, 2 KB buffers owned
//! by the driver. Frames are copied in and out of those buffers, so no caller
//! memory is ever mapped for DMA. CRC is stripped on receive and added and
//! padded to 64 bytes on transmit by the MAC.
//!
//! Region layout (all offsets page-relative, `DMA_BYTES` in total):
//! RX ring at 0 and TX ring at 512 (128-byte aligned, as RDBAL/TDBAL
//! require), then from 4096 the 32 RX buffers and the 32 TX buffers.

use super::{read, wait, write, Error, Registers, R, CTRL, HLREG0};
use core::ptr;
use core::sync::atomic::{fence, Ordering};

pub const RX_DESCS: usize = 32;
pub const TX_DESCS: usize = 32;
/// Buffer size: SRRCTL.BSIZEPACKET is in 1 KB units.
pub const BUF_SIZE: usize = 2048;
const RX_RING: usize = 0;
const TX_RING: usize = 512;
const RX_BUFS: usize = 4096;
const TX_BUFS: usize = RX_BUFS + RX_DESCS * BUF_SIZE;
pub const DMA_BYTES: usize = TX_BUFS + TX_DESCS * BUF_SIZE;
pub const DMA_PAGES: usize = DMA_BYTES.div_ceil(4096);

/// Largest frame without CRC: 1514 plus a 4-byte VLAN tag. The default
/// MAXFRS.MFS is 1518 with CRC, and the MAC allows a tagged frame 4 more.
pub const MAX_FRAME: usize = 1518;
/// Ethernet header: the smallest frame the driver will send.
pub const MIN_FRAME: usize = 14;

// Queue 0 registers (queues 0-63 at 0x40 steps).
const RDBAL: u32 = 0x01000;
const RDBAH: u32 = 0x01004;
const RDLEN: u32 = 0x01008;
const RDH: u32 = 0x01010;
const SRRCTL: u32 = 0x01014;
const RDT: u32 = 0x01018;
const RXDCTL: u32 = 0x01028;
const TDBAL: u32 = 0x06000;
const TDBAH: u32 = 0x06004;
const TDLEN: u32 = 0x06008;
const TDH: u32 = 0x06010;
const TDT: u32 = 0x06018;
const TXDCTL: u32 = 0x06028;
const RDRXCTL: u32 = 0x02f00;
const RXCTRL: u32 = 0x03000;
const DMATXCTL: u32 = 0x04a80;
const FCTRL: u32 = 0x05080;
const VLNCTRL: u32 = 0x05088;
const MTA: u32 = 0x05200;
pub const GPRC: u32 = 0x04074;
pub const GPTC: u32 = 0x04080;

const MASTER_DISABLE: u32 = 1 << 2;
const QUEUE_ENABLE: u32 = 1 << 25;
const RXEN: u32 = 1 << 0;
const TE: u32 = 1 << 0;
/// HLREG0.RXCRCSTRP and RDRXCTL.CRCSTRIP must agree.
const CRC_STRIP: u32 = 1 << 1;
const FCTRL_BAM: u32 = 1 << 10;
const FCTRL_MPE: u32 = 1 << 8;
const FCTRL_UPE: u32 = 1 << 9;
const VLNCTRL_VFE: u32 = 1 << 30;
/// SRRCTL: BSIZEPACKET 2 (KB), DESCTYPE 000 (legacy), DROP_EN.
const SRRCTL_VALUE: u32 = (BUF_SIZE / 1024) as u32 | 1 << 28;

// Legacy descriptor fields (byte offsets in the 16-byte descriptor).
const LENGTH: usize = 8;
const TX_CMD: usize = 11;
const STATUS_BYTE: usize = 12;
const RX_ERRORS: usize = 13;
const DD: u8 = 1 << 0;
const EOP: u8 = 1 << 1;
/// TX CMD: EOP, IFCS (insert CRC), RS (report status).
const TX_CMD_VALUE: u8 = 1 << 0 | 1 << 1 | 1 << 3;
/// RX errors: CE (CRC or symbol error) and RXE (other receive error).
const RX_BAD: u8 = 1 << 0 | 1 << 7;

/// The DMA region: `DMA_BYTES` at `host`, which the NIC reaches at `device`.
#[derive(Clone, Copy, Debug)]
pub struct Dma {
    pub host: *mut u8,
    pub device: u64,
}

/// What the receive filter passes (FCTRL); the station address in RAR0 is
/// always accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Filter {
    pub broadcast: bool,
    pub all_multicast: bool,
    pub promiscuous: bool,
}

/// A received frame's summary, for the console.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    pub len: usize,
    pub destination: [u8; 6],
    pub source: [u8; 6],
    pub ethertype: u16,
}

pub struct Rings {
    dma: Dma,
    rx_next: usize,
    tx_next: usize,
    tx_clean: usize,
}

fn low(a: u64) -> u32 { a as u32 }
fn high(a: u64) -> u32 { (a >> 32) as u32 }

impl Rings {
    /// # Safety
    /// `dma.host` must point to `DMA_BYTES` of memory that stays valid, and
    /// is used only through this value, until it is dropped; the NIC must
    /// reach it at `dma.device`.
    pub unsafe fn new(dma: Dma) -> Self {
        Rings { dma, rx_next: 0, tx_next: 0, tx_clean: 0 }
    }

    fn desc(&self, ring: usize, i: usize) -> *mut u8 {
        // SAFETY (for callers' volatile accesses): within the region (new).
        unsafe { self.dma.host.add(ring + i * 16) }
    }
    fn rx_buf(&self, i: usize) -> *mut u8 { unsafe { self.dma.host.add(RX_BUFS + i * BUF_SIZE) } }
    fn tx_buf(&self, i: usize) -> *mut u8 { unsafe { self.dma.host.add(TX_BUFS + i * BUF_SIZE) } }

    fn set_desc(&self, ring: usize, i: usize, addr: u64, second: u64) {
        let d = self.desc(ring, i) as *mut u64;
        // SAFETY: 16-byte descriptor in the region, 8-byte aligned.
        unsafe {
            ptr::write_volatile(d, addr);
            ptr::write_volatile(d.add(1), second);
        }
    }
    fn byte(&self, ring: usize, i: usize, at: usize) -> u8 {
        // SAFETY: within the descriptor; the NIC writes it back by DMA.
        unsafe { ptr::read_volatile(self.desc(ring, i).add(at)) }
    }
    fn length(&self, i: usize) -> usize {
        // SAFETY: as `byte`; 2-byte aligned.
        unsafe { ptr::read_volatile(self.desc(RX_RING, i).add(LENGTH) as *const u16) as usize }
    }

    fn arm_rx(&self, i: usize) {
        // Buffer address; length, status and errors cleared for write-back.
        self.set_desc(RX_RING, i, self.dma.device + (RX_BUFS + i * BUF_SIZE) as u64, 0);
    }

    /// Program queue 0 for receive and transmit and enable both (82599
    /// datasheet receive/transmit initialization). The MAC's reset left
    /// RX/TX off and interrupts masked; they stay masked (polled driver).
    pub fn start<Io: Registers>(&mut self, io: &mut Io, filter: Filter) -> R<(), Io::Error> {
        // quiesce set GIO master disable; the NIC must master again for DMA.
        let ctrl = read(io, CTRL)?;
        write(io, CTRL, ctrl & !MASTER_DISABLE)?;
        for i in 0..128 { write(io, MTA + 4 * i, 0)?; }
        let vln = read(io, VLNCTRL)?;
        write(io, VLNCTRL, vln & !VLNCTRL_VFE)?;
        self.set_filter(io, filter)?;
        let hl = read(io, HLREG0)?;
        write(io, HLREG0, hl | CRC_STRIP)?;
        let rdr = read(io, RDRXCTL)?;
        write(io, RDRXCTL, rdr | CRC_STRIP)?;

        // Receive queue 0.
        for i in 0..RX_DESCS { self.arm_rx(i); }
        self.rx_next = 0;
        let rx = self.dma.device + RX_RING as u64;
        write(io, RDBAL, low(rx))?;
        write(io, RDBAH, high(rx))?;
        write(io, RDLEN, (RX_DESCS * 16) as u32)?;
        write(io, SRRCTL, SRRCTL_VALUE)?;
        write(io, RDH, 0)?;
        write(io, RDT, 0)?;
        let v = read(io, RXDCTL)?;
        write(io, RXDCTL, v | QUEUE_ENABLE)?;
        wait(io, RXDCTL, QUEUE_ENABLE, QUEUE_ENABLE, 10)?;
        // RDT one behind RDH: all but one descriptor belong to the NIC.
        fence(Ordering::SeqCst);
        write(io, RDT, (RX_DESCS - 1) as u32)?;
        let v = read(io, RXCTRL)?;
        write(io, RXCTRL, v | RXEN)?;

        // Transmit queue 0.
        for i in 0..TX_DESCS { self.set_desc(TX_RING, i, 0, 0); }
        self.tx_next = 0;
        self.tx_clean = 0;
        let v = read(io, DMATXCTL)?;
        write(io, DMATXCTL, v | TE)?;
        let tx = self.dma.device + TX_RING as u64;
        write(io, TDBAL, low(tx))?;
        write(io, TDBAH, high(tx))?;
        write(io, TDLEN, (TX_DESCS * 16) as u32)?;
        write(io, TDH, 0)?;
        write(io, TDT, 0)?;
        // PTHRESH/HTHRESH/WTHRESH 0: write back each RS descriptor at once.
        write(io, TXDCTL, QUEUE_ENABLE)?;
        wait(io, TXDCTL, QUEUE_ENABLE, QUEUE_ENABLE, 10)?;
        Ok(())
    }

    /// FCTRL's broadcast, multicast-promiscuous and unicast-promiscuous bits.
    pub fn set_filter<Io: Registers>(&mut self, io: &mut Io, f: Filter) -> R<(), Io::Error> {
        let v = read(io, FCTRL)? & !(FCTRL_BAM | FCTRL_MPE | FCTRL_UPE);
        let bit = |on: bool, b: u32| if on { b } else { 0 };
        write(io, FCTRL, v | bit(f.broadcast, FCTRL_BAM) | bit(f.all_multicast, FCTRL_MPE)
            | bit(f.promiscuous, FCTRL_UPE))
    }

    /// Queue one frame (header included, no CRC). Ok(false): the ring is
    /// full; `reclaim` first.
    pub fn transmit<Io: Registers>(&mut self, io: &mut Io, frame: &[u8]) -> R<bool, Io::Error> {
        if frame.len() < MIN_FRAME || frame.len() > MAX_FRAME {
            return Err(Error::FrameLength { len: frame.len() });
        }
        self.reclaim();
        let i = self.tx_next;
        let next = (i + 1) % TX_DESCS;
        if next == self.tx_clean { return Ok(false); }
        // SAFETY: the TX buffer is BUF_SIZE >= MAX_FRAME bytes in the region,
        // and not owned by the NIC (i is past every descriptor it holds).
        unsafe { ptr::copy_nonoverlapping(frame.as_ptr(), self.tx_buf(i), frame.len()); }
        let second = frame.len() as u64 | (TX_CMD_VALUE as u64) << (8 * (TX_CMD - 8));
        self.set_desc(TX_RING, i, self.dma.device + (TX_BUFS + i * BUF_SIZE) as u64, second);
        self.tx_next = next;
        // Buffer and descriptor are in memory before the NIC is told.
        fence(Ordering::SeqCst);
        write(io, TDT, next as u32)?;
        Ok(true)
    }

    /// Frames the NIC has sent (DD written back) since the last call.
    pub fn reclaim(&mut self) -> usize {
        let mut n = 0;
        while self.tx_clean != self.tx_next && self.byte(TX_RING, self.tx_clean, STATUS_BYTE) & DD != 0 {
            self.tx_clean = (self.tx_clean + 1) % TX_DESCS;
            n += 1;
        }
        fence(Ordering::SeqCst);
        n
    }

    /// Frames queued and not yet sent.
    pub fn tx_pending(&mut self) -> usize {
        self.reclaim();
        (self.tx_next + TX_DESCS - self.tx_clean) % TX_DESCS
    }

    /// Hand descriptor `rx_next` back to the NIC and move on.
    fn recycle<Io: Registers>(&mut self, io: &mut Io) -> R<(), Io::Error> {
        let i = self.rx_next;
        self.arm_rx(i);
        self.rx_next = (i + 1) % RX_DESCS;
        fence(Ordering::SeqCst);
        write(io, RDT, i as u32)
    }

    /// Length of the next good received frame, if any. Errored frames and
    /// frames that did not fit one buffer are dropped on the way.
    pub fn next_len<Io: Registers>(&mut self, io: &mut Io) -> R<Option<usize>, Io::Error> {
        loop {
            let i = self.rx_next;
            let status = self.byte(RX_RING, i, STATUS_BYTE);
            if status & DD == 0 { return Ok(None); }
            // Read the rest of the descriptor only after DD.
            fence(Ordering::SeqCst);
            let len = self.length(i);
            if status & EOP != 0 && self.byte(RX_RING, i, RX_ERRORS) & RX_BAD == 0
                && (MIN_FRAME..=BUF_SIZE).contains(&len) {
                return Ok(Some(len));
            }
            self.recycle(io)?;
        }
    }

    /// Copy the next good frame into `out` and give its buffer back.
    /// Ok(None): nothing received. A frame longer than `out` stays queued
    /// and fails with `FrameLength` (its length), so the caller can retry.
    pub fn receive<Io: Registers>(&mut self, io: &mut Io, out: &mut [u8]) -> R<Option<usize>, Io::Error> {
        let Some(len) = self.next_len(io)? else { return Ok(None) };
        if len > out.len() { return Err(Error::FrameLength { len }); }
        // SAFETY: the NIC wrote `len` <= BUF_SIZE bytes and set DD; the
        // buffer is the driver's until `recycle`.
        unsafe { ptr::copy_nonoverlapping(self.rx_buf(self.rx_next), out.as_mut_ptr(), len); }
        self.recycle(io)?;
        Ok(Some(len))
    }

    /// Stop DMA: let queued frames go (up to 100 ms), disable TX queue 0,
    /// then `quiesce` (receive off, RX queues disabled, GIO master disable).
    /// Returns the frames left unsent.
    pub fn stop<Io: Registers>(&mut self, io: &mut Io) -> R<usize, Io::Error> {
        let mut left = self.tx_pending();
        for _ in 0..100 {
            if left == 0 { break; }
            io.delay_us(1000);
            left = self.tx_pending();
        }
        let v = read(io, TXDCTL)?;
        write(io, TXDCTL, v & !QUEUE_ENABLE)?;
        wait(io, TXDCTL, QUEUE_ENABLE, 0, 10)?;
        let v = read(io, DMATXCTL)?;
        write(io, DMATXCTL, v & !TE)?;
        super::quiesce(io)?;
        Ok(left)
    }
}

/// Summary of an Ethernet frame for the console.
pub fn frame(bytes: &[u8]) -> Frame {
    let mut destination = [0; 6];
    let mut source = [0; 6];
    destination.copy_from_slice(&bytes[0..6]);
    source.copy_from_slice(&bytes[6..12]);
    Frame { len: bytes.len(), destination, source, ethertype: u16::from_be_bytes([bytes[12], bytes[13]]) }
}

/// The DMA check frame: broadcast, from `mac`, EtherType 0x88B5 (IEEE
/// 802 local experimental), a text payload, 60 bytes (the MAC adds CRC).
pub fn check_frame(mac: [u8; 6]) -> [u8; 60] {
    let mut f = [0u8; 60];
    f[0..6].copy_from_slice(&[0xff; 6]);
    f[6..12].copy_from_slice(&mac);
    f[12..14].copy_from_slice(&0x88b5u16.to_be_bytes());
    let text = b"stormnic-ixgbe DMA check";
    f[14..14 + text.len()].copy_from_slice(text);
    f
}

/// What the DMA check saw (`check`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Checked {
    /// The NIC wrote back DD on the broadcast frame within 100 ms.
    pub sent: bool,
    /// Good frames received in the listening window, and the first one.
    pub received: usize,
    pub first: Option<Frame>,
    /// Milliseconds until the first frame, or the whole window.
    pub waited_ms: usize,
    /// GPTC and GPRC over the check (statistics are clear-on-read).
    pub gptc: u32,
    pub gprc: u32,
}

/// Send one broadcast frame and listen up to `listen_ms` for any frame
/// (broadcast, or to the station address). Rings must be started with
/// broadcast accepted.
pub fn check<Io: Registers>(io: &mut Io, rings: &mut Rings, mac: [u8; 6], listen_ms: usize)
    -> R<Checked, Io::Error> {
    // Clear the statistics so the counts below are this check's.
    io.read(GPTC).map_err(Error::Io)?;
    io.read(GPRC).map_err(Error::Io)?;
    rings.transmit(io, &check_frame(mac))?;
    let mut sent = false;
    for _ in 0..=100 {
        if rings.tx_pending() == 0 { sent = true; break; }
        io.delay_us(1000);
    }
    let mut buf = [0u8; BUF_SIZE];
    let (mut received, mut first, mut waited_ms) = (0, None, listen_ms);
    for ms in 0..=listen_ms {
        while let Some(len) = rings.receive(io, &mut buf)? {
            received += 1;
            if first.is_none() { first = Some(frame(&buf[..len])); }
        }
        if received > 0 { waited_ms = ms; break; }
        if ms < listen_ms { io.delay_us(1000); }
    }
    let gptc = io.read(GPTC).map_err(Error::Io)?;
    let gprc = io.read(GPRC).map_err(Error::Io)?;
    Ok(Checked { sent, received, first, waited_ms, gptc, gprc })
}
