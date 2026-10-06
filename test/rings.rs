//! Descriptor rings (#3) against the simulated NIC in `sim.rs`.
#[path = "../src/hardware.rs"]
#[allow(dead_code)]
mod hardware;
#[allow(dead_code)]
mod sim;
use hardware::rings::{self, Filter, BUF_SIZE, RX_DESCS, TX_DESCS};
use hardware::Error;
use sim::*;

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
fn default_check_does_not_wait_to_receive_and_verbose_listens_3_s() {
    assert_eq!((rings::listen_ms(false), rings::listen_ms(true)), (0, 3000));
    let (_mem, mut nic, mut rings) = started();
    let c = rings::check(&mut nic, &mut rings, MAC, rings::listen_ms(false)).unwrap();
    assert!(c.sent);
    assert_eq!((c.received, c.waited_ms, c.gptc), (0, 0, 1));
    // Only the TX descriptor wait: the frame is sent at once, so no delay.
    assert_eq!(nic.delays, 0);
    // A frame already in the ring is still seen by the one look (#24).
    let (_mem, mut nic, mut rings) = started();
    nic.loopback = true;
    let c = rings::check(&mut nic, &mut rings, MAC, rings::listen_ms(false)).unwrap();
    assert_eq!((c.received, c.waited_ms, c.gprc), (1, 0, 1));
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
