//! The SNP state machine and data path (#4) against the simulated NIC in
//! `sim.rs`: the calls the firmware's MNP makes, in its order, and each
//! call's failure statuses from the UEFI specification.
#[path = "../src/hardware.rs"]
#[allow(dead_code)]
mod hardware;
#[allow(dead_code)]
mod sim;
use hardware::rings::BUF_SIZE;
use hardware::snp::{self, Fail, Received, Snp, State};
use hardware::{Error, Family};
use sim::*;

const OTHER: [u8; 6] = [0x02, 9, 9, 9, 9, 9];
const MCAST: [u8; 6] = [0x01, 0x00, 0x5e, 0x00, 0x00, 0xfb];
const MCAST2: [u8; 6] = [0x33, 0x33, 0xff, 0x44, 0x55, 0x66];
type R<T> = Result<T, Fail<&'static str>>;

fn snp() -> (Vec<u8>, Nic, Snp) {
    let (mem, nic, rings) = setup();
    (mem, nic, Snp::new(rings, Family::F82599, MAC, true))
}

/// Start, Initialize and the filters MNP asks for: unicast + broadcast.
fn up() -> (Vec<u8>, Nic, Snp) {
    let (mem, mut nic, mut s) = snp();
    s.start::<&str>().unwrap();
    s.initialize(&mut nic).unwrap();
    s.receive_filters(&mut nic, snp::UNICAST | snp::BROADCAST, 0, false, &[]).unwrap();
    (mem, nic, s)
}

fn rx(s: &mut Snp, nic: &mut Nic) -> R<Received> {
    let mut buf = [0u8; BUF_SIZE];
    s.receive(nic, &mut buf)
}

#[test]
fn state_machine_follows_the_spec() {
    let (_mem, mut nic, mut s) = snp();
    assert_eq!(s.state, State::Stopped);
    assert_eq!(s.initialize(&mut nic), Err(Fail::NotStarted));
    assert_eq!(s.stop(&mut nic), Err(Fail::NotStarted));
    assert_eq!(s.shutdown(&mut nic), Err(Fail::NotStarted));
    assert_eq!(rx(&mut s, &mut nic), Err(Fail::NotStarted));
    s.start::<&str>().unwrap();
    assert_eq!(s.start::<&str>(), Err(Fail::AlreadyStarted));
    // Started, not initialized: DEVICE_ERROR for the data path.
    assert_eq!(s.get_status(&mut nic, true), Err(Fail::NotInitialized));
    assert_eq!(s.reset(&mut nic), Err(Fail::NotInitialized));
    assert_eq!(s.transmit(&mut nic, 0, &mut [0; 60], None, None, None, 1), Err(Fail::NotInitialized));
    s.initialize(&mut nic).unwrap();
    assert_eq!(s.state, State::Initialized);
    assert!(s.media);
    s.shutdown(&mut nic).unwrap();
    assert_eq!(s.state, State::Started);
    assert_ne!(nic.reg(CTRL) & (1 << 2), 0, "shutdown leaves GIO master disable set");
    s.initialize(&mut nic).unwrap();
    // Stop from initialized shuts down first.
    s.stop(&mut nic).unwrap();
    assert_eq!(s.state, State::Stopped);
    assert_eq!(nic.reg(RXCTRL) & 1, 0);
    assert_eq!(nic.reg(TXDCTL) & ENABLE, 0);
}

#[test]
fn initialize_programs_rar0_and_clears_filters() {
    let (_mem, mut nic, mut s) = snp();
    s.start::<&str>().unwrap();
    nic.regs.insert(RAH0, 0);
    s.initialize(&mut nic).unwrap();
    assert_eq!(nic.reg(RAL0), 0x3322_1102);
    assert_eq!(nic.reg(RAH0), 1 << 31 | 0x5544, "AV set");
    assert_eq!(s.setting, 0);
    assert_eq!(nic.reg(FCTRL) & (1 << 10 | 1 << 9 | 1 << 8), 0);
    assert_eq!(nic.reg(MCSTCTRL) & (1 << 2), 0, "MTA filter off");
    // Nothing enabled: even a unicast to the station is dropped.
    nic.deliver(&frame(MAC, 1, 60));
    assert_eq!(rx(&mut s, &mut nic), Err(Fail::NotReady));
}

#[test]
fn mnp_style_transmit_with_header_fill_and_recycling() {
    let (_mem, mut nic, mut s) = up();
    let mut buf = vec![0u8; 100];
    buf[14..18].copy_from_slice(b"ping");
    let token = buf.as_ptr() as usize;
    s.transmit(&mut nic, 14, &mut buf, None, Some(OTHER), Some(0x0800), token).unwrap();
    // The header was filled in the caller's buffer, source = station.
    assert_eq!(&buf[..6], &OTHER);
    assert_eq!(&buf[6..12], &MAC);
    assert_eq!(&buf[12..14], &[0x08, 0x00]);
    assert_eq!(nic.wire, vec![buf.clone()]);
    let (bits, got) = s.get_status(&mut nic, true).unwrap();
    assert_eq!(bits & snp::TRANSMIT_INTERRUPT, snp::TRANSMIT_INTERRUPT);
    assert_eq!(got, Some(token));
    // Once only.
    assert_eq!(s.get_status(&mut nic, true).unwrap(), (0, None));
}

#[test]
fn transmit_checks_its_parameters() {
    let (_mem, mut nic, mut s) = up();
    let t = 7;
    assert_eq!(s.transmit(&mut nic, 14, &mut [0; 60], None, None, Some(0x800), t), Err(Fail::InvalidParameter));
    assert_eq!(s.transmit(&mut nic, 14, &mut [0; 60], None, Some(OTHER), None, t), Err(Fail::InvalidParameter));
    assert_eq!(s.transmit(&mut nic, 12, &mut [0; 60], None, Some(OTHER), Some(0x800), t), Err(Fail::InvalidParameter));
    assert_eq!(s.transmit(&mut nic, 14, &mut [0; 10], None, Some(OTHER), Some(0x800), t), Err(Fail::BufferTooSmall(14)));
    assert_eq!(s.transmit(&mut nic, 0, &mut [0; 10], None, None, None, t), Err(Fail::BufferTooSmall(14)));
    assert_eq!(s.transmit(&mut nic, 0, &mut [0; 1519], None, None, None, t), Err(Fail::InvalidParameter));
    assert!(nic.wire.is_empty());
    // A complete frame (header 0) goes as given; a short one is padded by the MAC.
    let mut f = frame(OTHER, 1, 42);
    s.transmit(&mut nic, 0, &mut f, None, None, None, t).unwrap();
    assert_eq!(nic.wire, vec![frame(OTHER, 1, 42)]);
}

#[test]
fn full_ring_or_recycle_queue_is_not_ready() {
    let (_mem, mut nic, mut s) = up();
    nic.hold_tx = true;
    let mut n = 0;
    while s.transmit(&mut nic, 0, &mut frame(OTHER, n, 60), None, None, None, n as usize).is_ok() { n += 1; }
    assert_eq!(n as usize, hardware::rings::TX_DESCS - 1);
    assert_eq!(s.transmit(&mut nic, 0, &mut frame(OTHER, 0, 60), None, None, None, 0), Err(Fail::NotReady));
    nic.hold_tx = false;
    nic.process_tx();
    // The ring drains, but nobody recycled: the queue fills at RECYCLE.
    let mut m = n as usize;
    while s.transmit(&mut nic, 0, &mut frame(OTHER, 0, 60), None, None, None, m).is_ok() { m += 1; }
    assert_eq!(m, snp::RECYCLE);
    // GetStatus hands them back oldest first, then transmit works again.
    for want in 0..3 { assert_eq!(s.get_status(&mut nic, true).unwrap().1, Some(want)); }
    s.transmit(&mut nic, 0, &mut frame(OTHER, 0, 60), None, None, None, 99).unwrap();
}

#[test]
fn receive_fills_header_fields_and_keeps_a_frame_too_big_for_the_buffer() {
    let (_mem, mut nic, mut s) = up();
    nic.deliver(&frame(MAC, 5, 300));
    let (bits, _) = s.get_status(&mut nic, false).unwrap();
    assert_eq!(bits & snp::RECEIVE_INTERRUPT, snp::RECEIVE_INTERRUPT);
    assert!(s.frame_waiting(&mut nic));
    let mut small = [0u8; 100];
    assert_eq!(s.receive(&mut nic, &mut small), Err(Fail::BufferTooSmall(300)));
    let mut buf = [0u8; BUF_SIZE];
    let r = s.receive(&mut nic, &mut buf).unwrap();
    assert_eq!(r, Received { len: 300, destination: MAC, source: [0x02, 0, 0, 0, 0, 9], protocol: 0x0800 });
    assert_eq!(u16::from_be_bytes([buf[14], buf[15]]), 5);
    assert_eq!(s.receive(&mut nic, &mut buf), Err(Fail::NotReady));
    assert!(!s.frame_waiting(&mut nic));
}

#[test]
fn round_trip_through_loopback() {
    let (_mem, mut nic, mut s) = up();
    nic.loopback = true;
    let mut out = vec![0u8; 64];
    s.transmit(&mut nic, 14, &mut out, None, Some(MAC), Some(0x88b5), 1).unwrap();
    let r = rx(&mut s, &mut nic).unwrap();
    assert_eq!((r.len, r.destination, r.source, r.protocol), (64, MAC, MAC, 0x88b5));
}

#[test]
fn filters_are_exact_even_where_the_hardware_passes_more() {
    let (_mem, mut nic, mut s) = up();
    // Broadcast and unicast on; multicast off.
    for d in [[0xff; 6], MAC, MCAST] { nic.deliver(&frame(d, 0, 60)); }
    assert_eq!(rx(&mut s, &mut nic).unwrap().destination, [0xff; 6]);
    assert_eq!(rx(&mut s, &mut nic).unwrap().destination, MAC);
    assert_eq!(rx(&mut s, &mut nic), Err(Fail::NotReady));
    // Multicast list: MTA bit and MFE set, exact match in software.
    s.receive_filters(&mut nic, snp::MULTICAST, 0, false, &[MCAST]).unwrap();
    let (reg, bit) = snp::mta_bit(MCAST);
    assert_eq!(nic.reg(MTA + 4 * reg), 1 << bit);
    assert_ne!(nic.reg(MCSTCTRL) & (1 << 2), 0);
    assert_eq!(nic.reg(MCSTCTRL) & 3, 0, "MO = 00");
    // Same 12-bit hash, different address: the hardware passes it, the SNP drops it.
    let mut alias = MCAST;
    alias[2] = 0x5f;
    assert_eq!(snp::mta_bit(alias), snp::mta_bit(MCAST));
    for d in [alias, MCAST, MCAST2] { nic.deliver(&frame(d, 0, 60)); }
    assert_eq!(nic.dropped, 2, "MCAST before multicast was on; MCAST2, not in the MTA");
    assert_eq!(rx(&mut s, &mut nic).unwrap().destination, MCAST);
    assert_eq!(rx(&mut s, &mut nic), Err(Fail::NotReady));
    // Unicast off: the station's own frames go.
    s.receive_filters(&mut nic, 0, snp::UNICAST, false, &[]).unwrap();
    nic.deliver(&frame(MAC, 0, 60));
    assert_eq!(rx(&mut s, &mut nic), Err(Fail::NotReady));
    // Promiscuous passes everything, in hardware (UPE, MPE, BAM) too.
    s.receive_filters(&mut nic, snp::PROMISCUOUS, 0, false, &[]).unwrap();
    assert_eq!(nic.reg(FCTRL) & (1 << 10 | 1 << 9 | 1 << 8), 1 << 10 | 1 << 9 | 1 << 8);
    for d in [OTHER, MCAST2] { nic.deliver(&frame(d, 0, 60)); }
    assert_eq!(rx(&mut s, &mut nic).unwrap().destination, OTHER);
    assert_eq!(rx(&mut s, &mut nic).unwrap().destination, MCAST2);
    // All-multicast: MPE, no UPE.
    s.receive_filters(&mut nic, snp::PROMISCUOUS_MULTICAST, snp::PROMISCUOUS, true, &[]).unwrap();
    assert_eq!(nic.reg(FCTRL) & (1 << 9 | 1 << 8), 1 << 8);
    assert_eq!((s.mcast_count, nic.reg(MCSTCTRL) & (1 << 2)), (0, 0), "list reset, MFE off");
}

#[test]
fn receive_filters_checks_its_parameters() {
    let (_mem, mut nic, mut s) = up();
    assert_eq!(s.receive_filters(&mut nic, 0x20, 0, false, &[]), Err(Fail::InvalidParameter));
    assert_eq!(s.receive_filters(&mut nic, 0, 0x40, false, &[]), Err(Fail::InvalidParameter));
    // A list needs MULTICAST, multicast addresses, and at most 16.
    assert_eq!(s.receive_filters(&mut nic, 0, 0, false, &[MCAST]), Err(Fail::InvalidParameter));
    assert_eq!(s.receive_filters(&mut nic, snp::MULTICAST, 0, false, &[OTHER]), Err(Fail::InvalidParameter));
    assert_eq!(s.receive_filters(&mut nic, snp::MULTICAST, 0, false, &[MCAST; 17]), Err(Fail::InvalidParameter));
    assert_eq!(s.setting, snp::UNICAST | snp::BROADCAST, "unchanged by a refused call");
    s.receive_filters(&mut nic, snp::MULTICAST, 0, false, &[MCAST; 16]).unwrap();
    assert_eq!(s.mcast_count, 16);
}

#[test]
fn station_address_changes_rar0_and_reset_restores_the_nvm_address() {
    let (_mem, mut nic, mut s) = up();
    assert_eq!(s.station_address(&mut nic, false, None), Err(Fail::InvalidParameter));
    assert_eq!(s.station_address(&mut nic, false, Some(MCAST)), Err(Fail::InvalidParameter));
    s.station_address(&mut nic, false, Some(OTHER)).unwrap();
    assert_eq!((nic.reg(RAL0), nic.reg(RAH0)), (0x0909_0902, 1 << 31 | 0x0909));
    nic.deliver(&frame(OTHER, 0, 60));
    nic.deliver(&frame(MAC, 0, 60));
    assert_eq!(rx(&mut s, &mut nic).unwrap().destination, OTHER);
    assert_eq!(rx(&mut s, &mut nic), Err(Fail::NotReady));
    // Header source defaults to the new address.
    let mut buf = vec![0u8; 60];
    s.transmit(&mut nic, 14, &mut buf, None, Some([0xff; 6]), Some(0x806), 0).unwrap();
    assert_eq!(&buf[6..12], &OTHER);
    s.station_address(&mut nic, true, None).unwrap();
    assert_eq!(s.current, MAC);
    assert_eq!(nic.reg(RAL0), 0x3322_1102);
}

#[test]
fn reset_restarts_the_queues_keeping_filters_and_address() {
    let (_mem, mut nic, mut s) = up();
    s.receive_filters(&mut nic, snp::MULTICAST, 0, false, &[MCAST]).unwrap();
    s.station_address(&mut nic, false, Some(OTHER)).unwrap();
    s.reset(&mut nic).unwrap();
    assert_eq!(s.setting, snp::UNICAST | snp::BROADCAST | snp::MULTICAST);
    assert_eq!(s.current, OTHER);
    assert_ne!(nic.reg(MCSTCTRL) & (1 << 2), 0);
    assert_eq!(nic.reg(RXCTRL) & 1, 1);
    nic.deliver(&frame(MCAST, 3, 60));
    assert_eq!(rx(&mut s, &mut nic).unwrap().destination, MCAST);
}

#[test]
fn media_follows_links() {
    let (_mem, mut nic, mut s) = up();
    assert!(s.media);
    nic.regs.insert(LINKS, 0);
    s.get_status(&mut nic, false).unwrap();
    assert!(!s.media);
    nic.regs.insert(LINKS, 1 << 30 | 2 << 28);
    s.get_status(&mut nic, false).unwrap();
    assert!(s.media);
}

#[test]
fn a_queue_that_will_not_enable_fails_initialize_and_leaves_it_started() {
    let (_mem, mut nic, mut s) = snp();
    s.start::<&str>().unwrap();
    nic.stuck = Some(RXDCTL);
    assert!(matches!(s.initialize(&mut nic), Err(Fail::Device(Error::Timeout { register: RXDCTL, .. }))));
    assert_eq!(s.state, State::Started);
    assert_ne!(nic.reg(CTRL) & (1 << 2), 0, "not mastering");
}

#[test]
fn halt_at_exit_boot_services_stops_dma() {
    let (_mem, mut nic, mut s) = up();
    s.halt(&mut nic).unwrap();
    assert_eq!(s.state, State::Stopped);
    assert_eq!(nic.reg(RXCTRL) & 1, 0);
    assert_eq!(nic.reg(TXDCTL) & ENABLE, 0);
    assert_ne!(nic.reg(CTRL) & (1 << 2), 0);
    assert!(!s.frame_waiting(&mut nic));
}

#[test]
fn multicast_ip_to_mac_per_rfc_1112_and_2464() {
    let mut v4 = [0u8; 16];
    v4[..4].copy_from_slice(&[224, 0x80 | 1, 2, 3]);
    assert_eq!(snp::mcast_ip_to_mac(false, &v4), Some([0x01, 0x00, 0x5e, 0x01, 2, 3]));
    v4[0] = 192;
    assert_eq!(snp::mcast_ip_to_mac(false, &v4), None);
    let mut v6 = [0u8; 16];
    v6[0] = 0xff;
    v6[12..].copy_from_slice(&[0xff, 0x44, 0x55, 0x66]);
    assert_eq!(snp::mcast_ip_to_mac(true, &v6), Some(MCAST2));
    v6[0] = 0xfe;
    assert_eq!(snp::mcast_ip_to_mac(true, &v6), None);
}

#[test]
fn mta_hash_is_address_bits_47_to_36() {
    // Bits 47:36 = byte 5 (bits 47:40) and the high nibble of byte 4.
    assert_eq!(snp::mta_bit([0x01, 0, 0x5e, 0, 0xab, 0xcd]), (0xcda >> 5, 0xcda & 31));
    assert_eq!(snp::mta_bit([0x01, 0, 0, 0, 0x0f, 0]), (0, 0));
    assert_eq!(snp::mta_bit([0x01, 0, 0, 0, 0xf0, 0xff]), (127, 31));
}
