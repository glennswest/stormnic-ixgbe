# Descriptor rings and DMA (#3)

`src/rings.rs` (the `hardware::rings` module) gives the NIC one receive and
one transmit queue with DMA through `EFI_PCI_IO_PROTOCOL`. The SNP (#4,
[docs/snp.md](snp.md)) uses it for Transmit, Receive, GetStatus and ReceiveFilters. Like the rest of
`hardware`, it doesn't depend on UEFI: the caller passes in a mapped region
and `Registers`, and `test/rings.rs` runs it against a simulated NIC that
does DMA.

Source: the Intel 82599 10 GbE Controller Datasheet, the receive and transmit
initialization sequences, the legacy receive and transmit descriptor formats
and the queue registers. The X540 and X552 datasheets use the same registers
and layouts for queue 0. None of this came from another driver.

## Design

- **One region, driver-owned buffers.** `PciIo.AllocateBuffer` provides 33
  pages of boot-services data below 4 GB. They are mapped once with
  `Map(BusMasterCommonBuffer)`. Nothing else is ever mapped. Frames are copied
  into the TX buffers and out of the RX buffers, so a caller's memory is never
  given to the NIC. SNP's Transmit can then return at once, and its
  recycled-buffer pointer is just the caller's own pointer.

  | Offset | Contents |
  |---|---|
  | 0 | RX ring: 32 legacy descriptors of 16 B (RDBAL is 128-byte aligned) |
  | 512 | TX ring: 32 legacy descriptors of 16 B |
  | 4096 | 32 RX buffers of 2 KB |
  | 69632 | 32 TX buffers of 2 KB |

- **Legacy descriptors.** SRRCTL.DESCTYPE is 000. One 2 KB buffer holds any
  frame up to the default MAXFRS (1518 with CRC, plus a VLAN tag), so a frame
  never spans descriptors. A descriptor without EOP, or with the CE or RXE
  error bits, is dropped and handed back.
- **Polled.** Interrupts stay masked, as the reset left them. Whatever drives the SNP
  (stormbootx's smoltcp) polls it.
- **Queue 0 only.** MRQC stays at its reset value (no RSS), so every frame
  goes to queue 0.

## Start sequence (`Rings::start`)

1. Clear CTRL.GIO_MASTER_DISABLE. The quiesce before the reset set it, and
   the NIC has to master to fetch descriptors.
2. Zero the 128 MTA entries. Clear VLNCTRL.VFE, so there's no VLAN filtering.
   Set FCTRL: BAM for broadcast, MPE for all-multicast, UPE for promiscuous,
   as requested. RAR0 (the NVM MAC, AV set by the reset) always passes.
3. CRC strip: HLREG0.RXCRCSTRP and RDRXCTL.CRCSTRIP, which must agree. TX
   CRC insertion and short-frame padding (HLREG0.TXCRCEN/TXPADEN) keep their
   reset values (on).
4. RX queue 0: every descriptor gets its buffer address and a cleared
   status. Program RDBAL/RDBAH, RDLEN = 512 and SRRCTL (BSIZEPACKET 2 KB,
   legacy, DROP_EN), and set RDH = RDT = 0. Set RXDCTL.ENABLE and poll it
   back (10 ms), then write RDT = 31: the NIC owns every descriptor but one.
   Finally set RXCTRL.RXEN.
5. TX queue 0: set DMATXCTL.TE, program TDBAL/TDBAH, TDLEN = 512 and TDH =
   TDT = 0, then set TXDCTL.ENABLE with the prefetch, host and write-back
   thresholds at 0 (each RS descriptor is written back at once), and poll it
   back (10 ms).

Every descriptor or buffer write is followed by a fence before the tail
register write that hands it to the NIC. The status byte (DD) is read before
the rest of a written-back descriptor.

## Transmit, receive, stop

- `transmit(frame)`: 14 to 1518 bytes without CRC. It is copied into the
  next TX buffer and the descriptor is written (length, CMD = EOP | IFCS |
  RS), then TDT is bumped. It returns `false` when the ring is full: 31
  frames outstanding.
- `reclaim()` moves past every descriptor whose DD the NIC has written back.
- `receive(out)`: the next good frame. It is copied out and its descriptor
  goes back to the NIC (re-armed, then RDT). A frame longer than `out` stays
  queued and fails with `FrameLength { len }`, so SNP can return
  BUFFER_TOO_SMALL with the size.
- `stop()` gives queued frames up to 100 ms to go and reports how many never
  did. Then it clears TXDCTL.ENABLE (polled) and DMATXCTL.TE, and runs the
  bring-up `quiesce`: RXCTRL.RXEN and every RXDCTL.ENABLE off, GIO master
  disable set, and the master-enable status polled clear. After `stop` the
  NIC does no DMA, and `start` can run again.

## In Start (binding)

Start now enables PCI **bus mastering** along with memory decode (see
[bring-up notes](bring-up.md#memory-decode-and-bus-mastering-19)). Stop and
a failed Start undo that. After bring-up, Start:

1. allocates and maps the region. If that fails, Start fails with
   DEVICE_ERROR and the NIC is released;
2. starts the rings with broadcast accepted;
3. if the link is up, runs the **DMA check**:
   - it clears the GPTC/GPRC statistics by reading them;
   - it sends one 60-byte broadcast frame, from the NIC's MAC with EtherType
     0x88B5 (IEEE 802 local experimental) and the payload
     `stormnic-ixgbe DMA check`, and waits up to 100 ms for its DD;
   - on a verbose boot (#22) it listens up to 3 s for any frame (broadcast,
     or to the NIC's own MAC) and logs the first one. Otherwise it looks at
     the RX ring once and does not wait (#24): the SNP's first exchange
     (stormbootx's DHCP) exercises receive anyway, and the listen cost 3 s
     on every boot that linked;
   - it logs GPTC/GPRC;
4. stops the rings. Nothing DMAs until the SNP's Initialize starts them
   again, and its ExitBootServices event stops them, so no DMA runs into
   memory the OS could reuse. The rings move into the SNP (`snp::Port`,
   #4). Stop stops the rings again, then unmaps and frees the region.

If the queues can't be stopped (the device is gone, or a queue never
disables), the region is **never freed**: the NIC might still write to it.
Bus mastering is turned off with `Attributes(Disable, BUS_MASTER)`, and the
pages stay allocated until reboot.

A ring that fails to start (a queue enable timeout) fails Start after the
queues are stopped and the region released.

The DMA check is not a strict round trip: a switch doesn't send a
broadcast back to its sender. What it shows on the blade:
- **TX DMA**: the NIC fetched the descriptor and buffer and wrote DD back,
  and GPTC counted the frame;
- **RX DMA**: a frame from the wire landed in a buffer, with DD and a length
  written back, and GPRC agrees.

The receive half needs a verbose boot. Even then an empty 3 s window is
logged, not an error, and says little about the driver: the window starts
the moment the link comes up, and a switch port running spanning tree
forwards nothing while it is listening and learning (up to about 30 s
without PortFast/edge mode), and a quiet segment may send nothing anyway.
GPRC 0 says no good frame reached the MAC's receive path in the window; it
doesn't tell an idle wire from a port that isn't forwarding yet. Only the
switch port's counters for the same seconds can tell those apart. A frame that comes back as a reply to ours (a real round trip) needs
the SNP (#4) and a network stack on it: DHCP DISCOVER is a broadcast, and
the OFFER is the answer.

## Hardware checks (82599 SFP+, 8086:1557)

Expected SOL lines (the X9 blades' ports; the address differs per blade) after `link up 10000 Mb/s`, in a verbose boot (#22: these are trace lines; see README, "Console output"):

```
stormnic-ixgbe: 0000:03:00.0 8086:1557: DMA: 33 pages at device 0x…, RX 32 x 2048 B, TX 32 x 2048 B, legacy descriptors
stormnic-ixgbe: 0000:03:00.0 8086:1557: DMA check: broadcast frame sent, 60 bytes (GPTC 1)
stormnic-ixgbe: 0000:03:00.0 8086:1557: DMA check: received N frame(s) after M ms (GPRC N), first L bytes from … to ff:ff:ff:ff:ff:ff type 0806
  (or: DMA check: received nothing in 3000 ms (GPRC N))
stormnic-ixgbe: 0000:03:00.0 8086:1557: rings stopped
```

What to check:
1. `sent` and GPTC 1. `not sent within 100 ms` with the link up means TX DMA
   or queue setup is wrong.
2. Receive: the check that matters is the SNP's (stormbootx leases an
   address over it, docs/snp.md), not this window. In the window, GPRC
   should be at least the count received; GPRC > 0 with `received nothing`
   means frames reached the MAC but not the ring: look at SRRCTL, RDBA or
   RXEN. `received nothing ... (GPRC 0)` is not a failure (see above: a
   port not yet forwarding, or a quiet segment). A non-verbose boot logs
   `receive not listened for (verbose only; GPRC N)` instead.
3. Optionally, on another host on the segment,
   `tcpdump -e -i <if> ether proto 0x88b5` shows the check frame from the
   blade's MAC. That proves TX on the wire.
4. `rings stopped` with no `a frame was never sent`.

## Verification

`sc-build 'scripts/test-hardware.sh && scripts/check-driver.sh'` at 9abdc9a:
the 43 bring-up tests and 13 new ring tests passed. The release image is
x86_64 PE32+, subsystem 11, 88,576 bytes. The driver build has no warnings.

The simulated NIC (in `test/rings.rs` at 9abdc9a; since #4 in `test/sim.rs`,
shared with `test/snp.rs`):
- fetches TX descriptors and buffers through the device address on a TDT
  write, checks CMD, writes DD and counts GPTC;
- applies FCTRL and RAR0 filtering;
- writes RX buffers, length and DD|EOP at RDH, counts GPRC, and drops when
  RDH reaches RDT;
- asserts on any DMA outside the region, and on any descriptor handed to it
  that still holds a frame.

The tests cover:
- the register sequence and order;
- a broadcast frame going through both rings in loopback;
- the check on a quiet network, with frames from the network, and with a
  frame that is never sent;
- RX wrap over 115 frames in order, and a full RX ring dropping and
  recovering;
- errored and non-EOP frames dropped and handed back;
- frame-length limits, and a short buffer leaving the frame queued;
- TX ring full, reclaim and wrap;
- stop, the drain bound and a restart;
- the queue-enable timeouts;
- filters.

On hardware (server3, 8086:1557, 2026-10-01, this driver at 563ea8d):
check 1 passed (`broadcast frame sent, 60 bytes (GPTC 1)`); the listen saw
`received nothing in 3000 ms (GPRC 0)`. The rings themselves work: the SNP
received the DHCP exchange moments later and the blade booted, which is the
receive check. The empty window fits a switch port that was not yet
forwarding right after link-up; that was not confirmed against the switch's
counters. Since #24 the listen runs only on a verbose boot.
