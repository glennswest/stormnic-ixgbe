# CLAUDE.md — stormnic-ixgbe

A `no_std` UEFI driver giving firmware an `EFI_SIMPLE_NETWORK_PROTOCOL` for
Intel 82599 / X540 / X552 10 Gb Ethernet (the Linux `ixgbe` family). It's loaded by stormbootx from `\stormboot\drivers` on its boot
media, on machines whose firmware has the TCP/IP stack but no UEFI driver for
the NIC (the Supermicro X9 blades, stormbootx#26). Read README.md first.

Read the cross-project rules in `../CLAUDE.md` first. In particular, **build
with `sc-build` after pushing, never on this VM and never as root**, and
scratch files go in `tmp/`.

## Rules for this crate

- **Written from the vendor documentation** (Intel 82599 10 GbE Controller Datasheet; Intel Ethernet Controller X540 Datasheet; Intel X552 (Xeon D) datasheet), **not translated
  from iPXE or Linux.** The crate is MIT; a port of GPL code would not be.
  Reading them for behaviour is fine.
- It is a *driver*: the binary is an EFI boot-service driver
  (`/subsystem:efi_boot_service_driver`), not an application, and it
  installs `EFI_DRIVER_BINDING_PROTOCOL` so the firmware's `ConnectController`
  binds it. stormbootx loads it and then connects controllers.
- A platform's own driver must win. Supported tries a BY_DRIVER open of the NIC's
  `EFI_PCI_IO_PROTOCOL`; if another driver already holds it, decline.
- Everything the driver does is logged to the console. The only way to debug
  it on the blades is the SOL capture on stormcentral
  (`/var/lib/stormcentral/console/serverN/sol.log`).

## Build

```bash
sc-build scripts/check-driver.sh   # build + check PE32+ subsystem 11
sc-build 'cargo build --release --target x86_64-unknown-uefi'
```

No configuration, ports or APIs; the console lines are listed in README.md.

## How it ships

In stormbootx's `\stormboot\drivers`. Not a stormcentral component (no golden);
stormbootx builds it from a pinned commit once stormbootx#29 lands.

## Test

Once stormbootx#29 builds it, put the `.efi` in a stormbootx ISO's `\stormboot\drivers`, **without**
`ipxe-intelx.efi` (`scripts/build-boot-agent.sh --iso --drivers DIR` in
stormbootx). Boot server1 from the virtual CD, which the stormcentral minismbd
serves as `\boot\stormbootx.iso`. Ask the master to swap the ISO and boot
the blade; the master holds the BMC and console access.

## Version

`Cargo.toml` → `package.version`. Current: `v0.1.0`.

## Work plan

- [ ] Driver scaffold: `EFI_DRIVER_BINDING_PROTOCOL` (Supported/Start/Stop), matching the PCI IDs; build as an EFI boot-service driver (`/subsystem:efi_boot_service_driver`)
  - **Built, awaiting hardware (#1), 2026-09-28:** `sc-build scripts/check-driver.sh` passes at ed58e7c (x86_64 PE32+, subsystem 11 EFI_BOOT_SERVICE_DRIVER, 25600 bytes). Left: boot server1 with only this `.efi` in `\stormboot\drivers` and read the SOL log for `Supported`/`Start: bound` and the NIC's device ID. **Blocked on stormbootx#29**: nothing gets this crate's `.efi` onto the media yet (not a component, so no golden; sc-build keeps nothing; stormbootx's `build-nic-drivers.sh` builds only iPXE). Proposed there: build it from a pinned commit, opt-in, never beside `ipxe-intelx.efi` until #4. Then ask the master for the server1 boot.
  - **Done (#1):** `build.rs` sets `/SUBSYSTEM:EFI_BOOT_SERVICE_DRIVER`; `uefi::driver::install` puts the binding on the image handle; `src/pci_io.rs` defines `EFI_PCI_IO_PROTOCOL` from the UEFI spec (the `uefi` crate has none); `src/ids.rs` holds the 82599/X540/X552 device IDs; Supported reads the ID through PciIo (GetProtocol), then tries a `BY_DRIVER` open so a platform driver that already owns the NIC wins; Start keeps the `BY_DRIVER` open, Stop closes it. Every Intel NIC seen is logged, matched or not, so the first boot on server1 names its exact device ID (not yet known: the SOL log shows only `Intel(R) Boot Agent XE` and MAC ac:1f:6b:8a:a7:9c).
- [ ] Bring-up from the datasheet: reset, EEPROM/MAC read, link setup (SFP+ and 10GBASE-T), link status
  - **Scope resolved (#2), 2026-09-29:** The owner's 2026-09-28 issue comment requires all 22 matched PCI IDs, including X552 external PHY paths. Do not narrow support to the first blade. Common reset/NVM/link-status primitives remain at fed1c3b, remotely verified with 10 tests and the UEFI subsystem check; Start is still a scaffold.
  - **Documentation dependency (#2), 2026-09-29:** The public X552 datasheet names KR setup fields but the destination register addresses were not found; the CS4227 reference note points to separate vendor programming documents not located publicly. See docs/bring-up.md for exact sources and gaps. Owner input is needed to obtain vendor documentation usable for this MIT implementation; the all-22-ID decision remains unchanged. No driver code changed in this audit. Remote sc-build at 50b6bb5 passed all 10 existing tests and the subsystem-11 PE check; the local runs.jsonl append still failed on a read-only filesystem. This does not verify PHY bring-up.
  - **Done (#2), 2026-09-29, 025a137:** Start enables memory decode (attributes restored on Stop/failure), runs reset + NVM MAC, then 82599: NVM AUTOC/AUTOC2 + Restart_AN; X540: PHY negotiates from NVM, no writes; X552: logged as pending; bounded 3 s link wait, all logged. sc-build: 15 tests + subsystem-11 check (32,256 bytes). Details and known limits (SFP module-speed mismatch, flow control) in docs/bring-up.md.
  - **(Superseded by the next two entries) Blocked on owner (#2):** X552 PHY setup (15aa/15ab KR register addresses, 15ac CS4227, then X557/Marvell audit) needs the vendor documents asked for on #2 (needs-owner). Also not yet run on hardware: next server1 boot (8086:1557) should show `reset, LAN`, `link setup: NVM mode 10G SFI`, `link up 10000 Mb/s`. #2 stays open until both.
  - **Unblocked (#2), 2026-09-29:** owner answered "can you not look at the C source from linux?". Source chosen: Intel's own shared code as published BSD-3-Clause in FreeBSD `sys/dev/ixgbe` (ixgbe_x550.c, ixgbe_phy.c, ixgbe_x540.c, ixgbe_type.h, Copyright Intel) — vendor-authored and MIT-compatible, unlike the GPL Linux copy of the same code. Used for register addresses and sequences; the Rust is written here, and the BSD notice is kept in docs/bring-up.md. In progress: X552 per device — 15aa KX4 hardware-managed; 15ab KR (IOSF sideband KRM LINK_CTRL_1 advertise KR/KX + AN restart); 15ac SFP+ (I2C bit-bang, CS4227 reset check, SFP ID, CS4227 EDC mode, KR speed); 15ad X557 (MDIO cl.45 unstall, iXFI/KR internal link, match copper speed); 15ae 1G-T firmware-managed.
  - **Done (#2), 2026-09-29, 8407e3c:** X552 per-device setup in `src/x552.rs` as planned above. sc-build: 28 tests (13 new, incl. a bit-level I2C slave for CS4227/SFP EEPROM/port expander) + subsystem-11 check (53,248 bytes). Details, BSD notice and limits in docs/bring-up.md. Left for #2: nothing in code; hardware check on server1 (8086:1557, 82599 path): stormbootx#29 is done; filed stormbootx#44 to pin 884cf18 and boot a test ISO on **server2** (owner, 2026-09-29; same 8086:1557 at 03:00.0, links 10G under Linux; result goes in /var/lib/stormcentral/console/server2/sol.log), and #2 is proposed after it. No X552 machine known to test on.
- [x] Match the PHY/link code to `docs/spec/phy.md` (#13; owner/master 2026-09-29: check #2's code against the independent spec, not the shared code; fix differences; record spec section 10 as hardware checks for server1, stormbootx#45's rustnic media)
  - **Done (#13), 2026-09-29.** The code matches the spec (73ba6fb code, c174bcc tests). What was found and changed:
    - Structure: split into `src/{sync,mdio,i2c,sfp,f82599,x540,x552}.rs` under `hardware`, run in the spec's section 9 order (prepare before the MAC reset). The reset uses LNK_RST when the link is down and RST when it is up.
    - 82599: module ID, NVM init sequence, protected AUTOC write with pipeline reset, laser, rate select, multispeed, `setup_mac_link`, TN1010, QSFP.
    - X540: power-on and advertisement.
    - X552: X557 reset and advertisement, the copper watch, SFP multispeed, the CS4227 handshake waits.
    - I2C and SFP details, MNG_VETO everywhere, IDs 155c, 155d and 15b0.
  - The policy choices and deviations are in docs/bring-up.md: no semaphore force-take (hands off, LINKS only); no Intel-OUI rule; no SmartSpeed; no X552 LASI; no X552 crosstalk fix (no NVM path); no QSFP multispeed; 154f unbound.
  - sc-build at c174bcc: 43 tests plus the subsystem-11 check (81,408 bytes).
  - **Hardware checks** from spec section 10 are listed in docs/bring-up.md "Hardware checks" for server1/server2 (8086:1557): the NVM default LMS, SDP3 direction, SDP2 polarity, LINKS, module ID, SFI firmware version and reset type. The X540 and X552 items need hardware we don't have.
- [ ] Descriptor rings: legacy or advanced RX/TX descriptors, DMA buffers via `EFI_PCI_IO_PROTOCOL` Map/Unmap
  - **In progress (#3), 2026-09-29.** Plan, from the 82599/X540/X552 datasheets (receive/transmit initialization, legacy descriptor formats):
    - `src/rings.rs` (under `hardware`, UEFI-independent): one DMA region (RX ring 32 x 16 B, TX ring 32 x 16 B, 32 + 32 buffers of 2 KB, 33 pages) given as host pointer + device address. Legacy RX/TX descriptors (SRRCTL.DESCTYPE 0, BSIZEPACKET 2 KB). `start`: clear CTRL.MASTER_DISABLE, MTA cleared, FCTRL.BAM, CRC strip (HLREG0 + RDRXCTL), queue 0 RX (RDBA/RDLEN/SRRCTL, RXDCTL.ENABLE polled, RDT), RXCTRL.RXEN; DMATXCTL.TE, queue 0 TX (TDBA/TDLEN, TXDCTL.ENABLE polled). `transmit`/`reclaim`/`receive`; `stop`: TX drain + TXDCTL off, then `quiesce` (RX off, master disable).
    - `src/pci_io.rs`: AllocateBuffer/FreeBuffer/Map/Unmap wrappers; Start enables BUS_MASTER with MEMORY, maps the region BusMasterCommonBuffer, keeps it in `Bound` for #4; Stop/failure unmaps and frees.
    - Start's DMA check when the link is up: send one broadcast frame (EtherType 0x88B5), confirm TX DD, listen up to 3 s for any received frame, log it with GPTC/GPRC, then stop the queues (no DMA left running without an SNP; #4 adds the ExitBootServices stop).
    - `test/rings.rs`: a simulated device that DMAs from/to the region (TX ring fetch, loopback into the RX ring), run by `scripts/test-hardware.sh`.
    - Hardware: the blade check is the same stormbootx media pin as #2 (stormbootx#47).
  - **Done in code (#3), 2026-09-29, 9abdc9a** as planned; docs/rings.md. sc-build: 43 + 13 new ring tests, subsystem-11 check (88,576 bytes), no driver warnings. Left: the blade check (docs/rings.md "Hardware checks": `DMA check: broadcast frame sent`, `received N frame(s)`) needs the rustnic media pinned at this commit; asked in stormbootx, and #3 proposed after it.
- [ ] `EFI_SIMPLE_NETWORK_PROTOCOL`: Start/Stop/Initialize/Reset/Shutdown, ReceiveFilters (unicast, broadcast, multicast), Transmit/Receive, GetStatus, StationAddress; install on a child handle with a MAC device path
  - **In progress (#4), 2026-09-29.** Plan (UEFI spec SNP chapter; 82599/X540/X552 datasheets for RAR0, MTA/MCSTCTRL):
    - `src/snp_core.rs` (`hardware::snp`, UEFI-independent, tested): the SNP state machine (Stopped/Started/Initialized) over `Rings` — start/stop/initialize/reset/shutdown, receive_filters (FCTRL BAM/MPE/UPE; multicast list into the MTA with MCSTCTRL.MFE, MO=00 bits 47:36; a software filter on receive so the setting is exact), station_address (RAR0 with AV), mcast_ip_to_mac, transmit (header fill, copy, the caller's buffer queued for GetStatus recycling), receive (header fields, BUFFER_TOO_SMALL keeps the frame), get_status (RECEIVE/TRANSMIT bits, media from LINKS, one recycled buffer). Statistics and NvData UNSUPPORTED.
    - `src/snp.rs`: the `EFI_SIMPLE_NETWORK_PROTOCOL` interface and Mode (uefi-raw layout), each call at TPL_CALLBACK; WaitForPacket (NOTIFY_WAIT); a child handle with the controller's device path + a MAC node (messaging 3/11, IfType 1); PciIo opened BY_CHILD_CONTROLLER; an ExitBootServices event that stops the queues.
    - `src/binding.rs`: own driver-binding glue (the `uefi` crate's `driver::install` refuses Stop with children), Stop with children uninstalls the child; Stop with none releases the NIC.
    - `test/snp.rs` against the simulated DMA NIC; `scripts/test-hardware.sh` runs it.
    - Hardware: the blade check is the issue's acceptance (`tcp4 : available` on server1 with only this driver), which needs stormbootx media pinned at the commit.
  - **Done in code (#4), 2026-09-29, 85a1c2a..e128db4** as planned; docs/snp.md. sc-build at e128db4: 43 + 13 + 16 new SNP tests, subsystem-11 check (97,792 bytes), no warnings. Left: the acceptance boot on server1 (docs/snp.md "Hardware check": `SNP initialized`, then stormbootx `tcp4 : available`) with only this driver on the media; needs the stormbootx media pinned at this commit, and the master for the boot. The UEFI glue (child handle, device path, TPL, events, Stop with children) is only tested by that boot.
- [ ] Test on server1 (X9 blade): only this driver in `\stormboot\drivers`, stormbootx prints `tcp4 : available` and claims its boothost
- [x] Commit `Cargo.lock` (#11): generated on dev through sc-build, committed at 7aac38c (uefi 0.39.0, uefi-raw 0.15.1); `scripts/check-driver.sh` builds `--locked`; sc-build at 7aac38c passed tests + subsystem-11 check with the lock unchanged. Byte-identical output across jobs is a separate issue (#12).
- [ ] Retire `ipxe-intelx.efi` from the stormbootx media (stormbootx#27)
