# Simple Network Protocol (#4)

Start installs an `EFI_SIMPLE_NETWORK_PROTOCOL` for each NIC it binds, on a
**child handle** with a **MAC address device path**. stormbootx (v0.9.0 and
later, stormbootx#56) opens that SNP `EXCLUSIVE`, which makes the firmware
disconnect any MNP bound to it, and runs its own TCP/IP stack, smoltcp,
directly on the SNP calls. It does not use the firmware's `EFI_TCP4`. The
SNP is a plain UEFI SNP, so on other firmware paths the firmware's MNP, IP4
and TCP4 can still bind on top of it.

Source: the UEFI specification's Simple Network Protocol section (the
calls, their statuses, the Mode fields, the state machine) and the driver
model sections (child handles, BY_CHILD_CONTROLLER, Stop with children).
The receive-filter registers (FCTRL, RAR0, MTA, MCSTCTRL) come from the
82599 datasheet; the X540 and X552 datasheets have the same registers.
No other driver's code was used.

## Files

- `src/snp_core.rs` (`hardware::snp`): the state machine and data path over
  the rings (`docs/rings.md`). It doesn't depend on UEFI; `test/snp.rs`
  runs it against the simulated NIC in `test/sim.rs`.
- `src/snp.rs`: the protocol interface and Mode (the `uefi-raw` layouts),
  the child handle, the device path, the events, and the mapping of each
  failure to an EFI status.
- `src/binding.rs`: the driver-binding glue. The `uefi` crate's
  `driver::install` answers UNSUPPORTED to Stop with children, which
  DisconnectController needs once there is a child, so the driver installs
  its own `EFI_DRIVER_BINDING_PROTOCOL`.

## The child handle

After bring-up and the DMA check, Start:

1. builds the child's device path: the controller's path (opened
   GET_PROTOCOL), then a MAC node (messaging type 3, subtype 11, 37 bytes:
   the address padded to 32 bytes and IfType 1, Ethernet), then the end
   node;
2. creates the WaitForPacket event (NOTIFY_WAIT, TPL_NOTIFY) and an
   ExitBootServices event (TPL_CALLBACK);
3. installs the SNP and the device path on a new handle with
   `InstallMultipleProtocolInterfaces`;
4. opens the controller's PciIo **BY_CHILD_CONTROLLER** from the child, so
   the firmware knows the child belongs to the controller.

Any failure undoes the steps before it, releases the DMA region and fails
Start with DEVICE_ERROR (`SNP not installed: STATUS; releasing`).

Stop is called twice by DisconnectController: first with the child, which
closes the child's PciIo open and uninstalls its protocols (if a consumer
still has the SNP open, the uninstall fails, the open is put back and Stop returns
DEVICE_ERROR); then with no children, which stops the queues, closes the
events, frees the Port, unmaps and frees the DMA region, undoes the PCI
attribute and command-register changes and closes PciIo BY_DRIVER.

## Mode

| Field | Value |
|---|---|
| State | Stopped, Started or Initialized, after every call |
| HwAddressSize, MediaHeaderSize, MaxPacketSize | 6, 14, 1500 |
| NvRamSize, NvRamAccessSize | 0, 0 |
| ReceiveFilterMask | 0x1f: unicast, multicast, broadcast, promiscuous, promiscuous multicast |
| ReceiveFilterSetting | the enabled bits |
| MaxMCastFilterCount, MCastFilterCount, MCastFilter | 16, the list's length, the list |
| CurrentAddress | the station address (RAR0) |
| BroadcastAddress | ff:ff:ff:ff:ff:ff |
| PermanentAddress | the NVM MAC read at reset |
| IfType | 1 (Ethernet) |
| MacAddressChangeable, MultipleTxSupported, MediaPresentSupported | TRUE |
| MediaPresent | LINKS.LINK_UP, read in Start, Initialize, Reset and GetStatus |

## The calls

Every call raises to TPL_CALLBACK, the spec's limit for SNP, so a caller's
timer poll (MNP's, or one driving smoltcp) never enters the driver while another call is running. A call on
a stopped interface returns NOT_STARTED. The data-path calls on a started
but uninitialized interface return DEVICE_ERROR.

| Call | What it does |
|---|---|
| Start | Stopped to Started (ALREADY_STARTED otherwise) |
| Stop | Started to Stopped. An initialized interface is shut down first |
| Initialize | Starts the queues (`Rings::start`), writes RAR0 with the current address (AV set), clears every receive filter and the multicast list, reads the link. On an initialized interface it re-initializes. The extra buffer sizes are ignored: the buffers are the driver's own |
| Reset | Stops and restarts the queues, keeping the filters, the list and the station address |
| Shutdown | Stops the queues (`Rings::stop`: TX drained up to 100 ms, RX and TX off, GIO master disable). Initialized to Started |
| ReceiveFilters | New setting = (setting \| Enable) & !Disable. Bits outside the mask are INVALID_PARAMETER. `ResetMCastFilter` empties the list; otherwise a non-empty list replaces it: at most 16 multicast addresses, with MULTICAST on, or INVALID_PARAMETER |
| StationAddress | `Reset` restores the NVM address; otherwise the new unicast address goes into RAR0 |
| Statistics | UNSUPPORTED |
| MCastIpToMac | IPv4 224.0.0.0/4 to 01:00:5e + the low 23 bits; IPv6 ff00::/8 to 33:33 + the low 32 bits; anything else INVALID_PARAMETER |
| NvData | UNSUPPORTED |
| GetStatus | Reclaims sent descriptors and reads LINKS into MediaPresent. InterruptStatus: RECEIVE if a frame is waiting, TRANSMIT if one went out since the last call. TxBuf: the oldest transmitted buffer not yet handed back, or NULL. Either pointer may be NULL |
| Transmit | Frames up to 1518 bytes (1514 plus a VLAN tag). With HeaderSize 14 the header is filled in the caller's buffer: destination and protocol required, source defaulting to the station address. The frame is copied into a ring buffer, and the caller's buffer is queued for GetStatus. NOT_READY when the ring (31 frames) or the recycle queue (64 buffers) is full |
| Receive | The next frame the filters pass: its length, header size 14, source, destination and protocol. BUFFER_TOO_SMALL sets `*BufferSize` to the frame's length and leaves the frame queued. NOT_READY when nothing is waiting |

WaitForPacket is signalled when its check finds a frame the filters pass.

### Receive filters

The hardware passes a superset of the setting, and Receive drops what the
setting doesn't allow, so the filters are exact:

| Setting | Hardware |
|---|---|
| UNICAST | RAR0 always passes the station address. With UNICAST off those frames are dropped in software |
| BROADCAST | FCTRL.BAM |
| MULTICAST with a list | MTA bits for each address and MCSTCTRL.MFE, MO = 00 (address bits 47:36: the last byte and the high nibble of the one before; the upper 7 bits pick one of the 128 registers, the lower 5 the bit). A different address with the same 12-bit hash passes the MTA and is dropped in software |
| PROMISCUOUS_MULTICAST | FCTRL.MPE |
| PROMISCUOUS | FCTRL.UPE, MPE and BAM |

### Transmit buffer recycling

The spec lets a driver keep the caller's buffer until GetStatus returns it.
This driver copies the frame, so the buffer is free at once, but it is
still handed back through GetStatus in order: callers such as MNP wait for
that before they reuse a buffer. A caller that never calls GetStatus gets NOT_READY after
64 frames.

## ExitBootServices

The ExitBootServices event stops the queues on an initialized interface
(`Snp::halt`): after that the NIC does no DMA into memory the OS now owns.
It prints nothing, because the OS may already have the console.

## Console lines

| Line | When |
|---|---|
| `stormnic-ixgbe: LOC 8086:DDDD NAME: Start: bound, SNP on a child handle, MAC xx:xx:xx:xx:xx:xx, media present\|absent` | Start, success |
| `stormnic-ixgbe: LOC 8086:DDDD: SNP not installed: STATUS; releasing` | Start, the child handle could not be made (DEVICE_ERROR) |
| `stormnic-ixgbe: LOC: SNP initialized, MAC xx:xx:xx:xx:xx:xx, media present\|absent` | Initialize (the first use: stormbootx's smoltcp) |
| `stormnic-ixgbe: LOC: SNP receive filters 0xNN, N multicast address(es)` | ReceiveFilters changed the setting or set a list |
| `stormnic-ixgbe: LOC: SNP station address xx:xx:xx:xx:xx:xx` | StationAddress |
| `stormnic-ixgbe: LOC: SNP shut down` | Shutdown |
| `stormnic-ixgbe: LOC: SNP CALL failed: ERROR` | a call failed in the NIC (DEVICE_ERROR), e.g. `Timeout { register: 1028, .. }` |
| `stormnic-ixgbe: LOC: Stop: SNP child removed` | Stop with the child |
| `stormnic-ixgbe: LOC: Stop: SNP child still in use (STATUS); kept` | Stop, uninstall refused (DEVICE_ERROR) |

Transmit, Receive and GetStatus print nothing: the network stack calls them
many times a second.

## Hardware check (the acceptance for #4)

The acceptance, restated for stormbootx v0.9.0 and later (#18): with only
this driver for the Intel NIC on the media, stormbootx prints `tcp4 :
smoltcp over SNP (nic N MAC)`, gets a DHCP lease and claims its boothost.
(Before v0.9.0 the acceptance was the firmware's `tcp4 : available`; that
line no longer appears.)

**Passed on server3, 2026-10-01** (X9SRD-F, 82599EN SFP+ 8086:1557; rustnic
media `golden-stormbootx-rustnic-416b7237c78a29a3`, this driver at
563ea8d). The SOL log (`/var/lib/stormcentral/console/server3/sol.log`),
`stormnic-ixgbe: LOC 8086:1557:` prefixes left out:

```
PCI attributes Get 0x700, Supported 0x8000000000078763, Enable 0x2: SUCCESS; command 0x0007
reset (RST), LAN 0, MAC ac:1f:6b:8a:a4:5c
EEMNGCTL CFG_DONE0 not set after 1 s (EEMNGCTL 0x80000196); NVM auto-read done, continuing
link setup: module passive DA, multispeed (...), NVM init sequence 79 words, 10G SFI (NVM AUTOC c09c6084, now AUTOC c09c6084 AUTOC2 000a0000), laser on
SFI firmware patch version 0x107
link up 10000 Mb/s
82599EN SFP+: Start: bound, SNP on a child handle, MAC ac:1f:6b:8a:a4:5c, media present
SNP initialized, MAC ac:1f:6b:8a:a4:5c, media present
```

stormbootx then printed `tcp4 : smoltcp over SNP`, `nic 0: leased
192.168.16.104/20`, reached the engine and claimed boothost/server3. That
boot exercised the UEFI glue the simulated tests don't: the child handle,
device path, events, TPL, an `EXCLUSIVE` open of the child's SNP and
smoltcp's call order. Not yet seen: Stop with the child, when stormbootx
gives NICs back to the firmware on its fall-through (stormbootx#68).

If a later boot fails:
- no `SNP initialized`: nothing opened the child's SNP. Check that the
  firmware ran ConnectController recursively over the new handle and that
  stormbootx found the SNP;
- `SNP initialized` but no lease: look at `media` (LINKS) and the
  `DMA check` lines; a `SNP ... failed` line names the register. The filter
  setting is the caller's choice; for IPv4 it should include unicast 0x01
  and broadcast 0x04.

## Verification

`sc-build` at e128db4: the 43 bring-up tests, 13 ring tests and 16 new SNP
tests passed (`scripts/test-hardware.sh`), and `scripts/check-driver.sh`
built an x86_64 PE32+, subsystem 11, 97,792-byte image with no warnings.

`test/snp.rs` covers:
- the state machine and each state's failure status;
- Initialize programming RAR0 and clearing the filters;
- transmit with the header filled in by the driver: the header filled in the caller's buffer, the
  TRANSMIT bit, the buffer handed back once;
- Transmit's parameter checks;
- NOT_READY from a full ring and from a full recycle queue, and recovery;
- Receive's header fields, and BUFFER_TOO_SMALL keeping the frame;
- a loopback round trip;
- exact filtering where the hardware passes more (an MTA hash alias,
  unicast off), promiscuous and all-multicast;
- ReceiveFilters' parameter checks;
- StationAddress into RAR0, the header source following it, and reset to
  the NVM address;
- Reset keeping filters and address;
- MediaPresent following LINKS;
- a queue that won't enable failing Initialize and leaving the NIC not
  mastering;
- the ExitBootServices halt;
- MCastIpToMac and the MTA hash.

The simulated NIC now applies RAR0 from its registers and the MTA with
MCSTCTRL.MFE, as well as FCTRL.

The UEFI glue in `src/snp.rs` and `src/binding.rs` (the child handle,
device path, events, TPL, Stop with children) is not covered by the
simulated tests; the server3 boot above is its test, Stop with children
excepted.
