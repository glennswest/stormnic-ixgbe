# Bring-up implementation notes (#2, #13)

Start runs the sequence below through BAR0 (`src/binding.rs` →
`src/hardware.rs` and its modules). The owner requires every matched PCI ID
(decision recorded on #2 on 2026-09-28). Reset and the NVM MAC follow the
datasheets. **PHY and link programming follows `docs/spec/phy.md`**, the
independent specification written from Intel's BSD-licensed shared code (#13).
See [PHY and link programming per the spec](#phy-and-link-programming-per-the-spec-13-2026-09-29),
which supersedes the earlier 82599/X540 datasheet link setup and the first
X552 implementation described further down. The 82599 SFP+ path has run
on hardware (server3, 8086:1557, 2026-10-01: 10G SFI link on a passive DA
cable); the X540 and X552 paths have not. See
[Hardware checks](#hardware-checks-spec-section-10).

## PHY and link programming per the spec (#13, 2026-09-29)

The owner asked for #2's PHY code (8407e3c) to be checked **against
`docs/spec/phy.md`**, not against the shared code, and fixed where it
differs. The check found that the 82599 and X540 paths were far from the spec,
and that the X552 paths differed in order and detail. The code was
restructured to follow the spec's section 9 walkthroughs. Comments cite spec
sections ("spec 5.4").

### Modules

| File | Spec | What |
|---|---|---|
| `src/hardware.rs` | 1.3, 1.5, 5.13, 6.7, 7.11, 8, 9 | `begin` (port, quiesce), `veto` (MMNGC once), `prepare`, `reset` (per family), `nvm_word` (EERD), `setup_link`, `link`, `cage_link` (crosstalk fix, 11.4), `wait_link` |
| `src/sync.rs` | 1.4 | SW/FW semaphores: 82599 SMBI+SWESMBI, X540/X552 SMBI+REGSMP; 200 × 5 ms (82599, X540) or 1000 × 5 ms (X552); release delays; X552 port-1 I2C mux |
| `src/mdio.rs` | 2 | clause 45 access per register under the port's PHY semaphore; probe/scan (ID with revision masked); generic and X557 PHY reset; 1.0x0004 abilities; the 6.5 advertisement and AN restart; 7.0x0001 read twice |
| `src/i2c.rs` | 3 | bit-bang with both I2CCTL layouts (0x28 and 0x15F5C); ACK sampled 10 × 1 µs; byte reads 11 (82599) / 4 (X552) attempts, locked per attempt with 100 ms after a failure; writes 2 attempts; 82599 QSFP bus handshake; CS4227 combined read (checksum byte NACKed) and write |
| `src/sfp.rs` | 4, 11.3 | SFF-8472 identification in the spec's order (identifier re-read up to 5 times, a failed read is "not present", 10G-BX before BX10), multispeed, support rule, NVM key; QSFP and its multispeed rule; soft rate select |
| `src/f82599.rs` | 5, 9.1–9.4, 11.1, 11.3 | media by device ID (154f: LCO, the backplane path); NVM init sequence into CORECTL under MAC_CSR; protected AUTOC write with pipeline reset and LESM; capabilities; `setup_mac_link`; multispeed; laser; hard/soft rate select; crosstalk link check; TN1010 |
| `src/x540.rs` | 6, 9.5 | scan, power on (30.0x0000 bit 11), advertise all abilities, restart AN unless vetoed |
| `src/x552.rs` | 7, 9.6–9.9, 11.2, 11.4 | NVM word through the firmware host interface; IOSF sideband, KR AN, iXFI, mux, CS4227 check-and-reset, SFP per-speed step and multispeed, X557 unstall/reset/advertise, copper watch |

### Order (spec 9)

1. `begin`: STATUS.LAN_ID, then quiesce (datasheet sequence, unchanged).
2. `veto`: MMNGC.MNG_VETO, read once. While set: no PHY reset, no AN restart,
   no 82599 AUTOC write, no X552 KR setup. The laser is still enabled.
3. `prepare`, the steps the spec puts before the MAC reset:
   - 82599 SFP+ and bypass: module ID. QSFP: bus handshake set-up, then QSFP ID.
   - 82599 T3: scan, then TN1010 reset unless vetoed or over-temperature.
   - X552 15ac: mux set-up, CS4227 check-and-reset, module ID.
   - X552 15ad: MDCSPD cleared, X557 probe, unstall, and reset unless vetoed.
4. `reset`: CTRL.LNK_RST when LINKS is down and CTRL.RST when it is up, never
   both. The X540 always uses RST.
   - Semaphore: the X540 and X552 15ad hold the port's PHY bit; X552 15ac
     holds 0x1806 with the mux.
   - Timing: still no access in the first millisecond, then completion within
     100 ms, then 50 ms (100 ms on the X540).
   - Then the NVM waits and RAR0 as before. On the 82599, AUTOC2 link-disable
     bits are cleared. On the X552, MDCSPD is cleared again (15ad) and the mux
     set-up redone (15ac).
5. `setup_link`, per family (below).
6. `wait_link`: every 100 ms, up to 9 s for copper (X540, 82599 T3, X552 15ad)
   and 3 s otherwise. X552 15ad reports up only when LINKS and the X557 both
   say so. It re-forces the internal link on each copper link-up or speed
   change. With the crosstalk fix (82599 SFP+ and QSFP+: SDP2; X552 15ac:
   SDP0) an empty cage is down, and "up" is confirmed 5 ms later.

A PCI I/O error or a removed device stops bring-up (DEVICE_ERROR).

Any other failure in `prepare` or `setup_link` is logged and link setup is
skipped: a semaphore held by firmware, no PHY, an I2C or sideband error, or
no NVM init sequence. Start still reports LINKS ("hands off", the spec 1.4.3
recommendation).

### Per family

- **82599 SFP+ (10fb, 1507, 1529, 154a, 154d, 1557) and bypass (155d)**:
  1. Module ID over I2CCTL 0x28 under the port's PHY semaphore. An absent or
     unknown module is logged, and nothing is set up.
  2. After the reset, NVM[0x2B] gives the init-sequence list. The key is
     3 + lan for passive DA and 5 + lan otherwise. The list is walked, and the
     data block goes to CORECTL under MAC_CSR, followed by 10 ms.
  3. Protected write of AUTOC = NVM AUTOC | LMS 011: MAC_CSR if LESM is on,
     then the pipeline reset. The pipeline reset toggles LMS bit 2 with
     Restart_AN, polls ANLP1 10 × 4 ms, then writes the value back.
  4. Laser (SFP+ only): SDP3 is cleared, then 100 ms. That is skipped when
     manageability is enabled or ESDP bit 11 (SDP3_DIR) is clear.
  5. Capabilities by spec 5.6.
  6. Multispeed modules (and bypass): 10G, then 1G, then 10G again. Each try
     does a rate select (SDP5 hard; bypass soft), 40 ms, `setup_mac_link`,
     and the laser flap on the first try. Single-speed modules: one
     `setup_mac_link`.
  7. The SFI firmware patch version (spec 5.12) is logged.
- **82599 QSFP (1558)**: as SFP+ with the QSFP ID and no rate select or laser.
  Multispeed (spec 11.3) for 1G SX + 10G SR or 1G LX + 10G LR modules (not
  DA): 10G, 1G (SFI without AN), 10G, as for SFP+ but with no rate select or
  flap. The crosstalk fix covers QSFP too (spec 11.4).
- **82599 LS (154f)**: the shared code's "fiber LCO" media (spec 11.1): no
  module ID, laser or rate select, so the backplane path below.
- **82599 backplane and CX4 (10f7, 10f8, 10f9, 10fc, 1514, 1517, 152a)**:
  `setup_mac_link` with the NVM capabilities and the KX_AN_COMP wait. With the
  NVM's own advertisement this writes nothing. SmartSpeed is not used, which
  spec 9.2 allows.
- **82599 T3 (151c)**:
  1. TN1010 advertisement: 7.0x0020 bit 12, 7.0x0017 bit 14, 7.0x0010 bit 8.
  2. AN restart and a pipeline reset, unless vetoed.
  3. 50 ms.
- **X540 (1528, 1560, 155c)**:
  1. Scan.
  2. PHY out of low-power mode.
  3. Advertise 1.0x0004's speeds.
  4. AN restart unless vetoed.
- **X552 15ab KR**: KR+KX AN and restart, unless vetoed.
- **X552 15aa KX4, 15b0 XFI, 15ae 1G-T**: nothing.
- **X552 15ac SFP+**: for a supported module, NVM word 0x2C is read through
  the firmware host interface (spec 11.2: FLEX_MNG command 0x31, HICR.C, under
  SW_MNG + EEP); bit 7 clear turns on the cage check (SDP0) for the
  multispeed polls and the link wait. If the host interface is disabled,
  fails, or its semaphore is held, the console says so and the check stays
  off. Multispeed modules run 10G/1G/10G with soft rate select.
  Each speed sets the KR AN for that speed only and writes the CS4227 EDC (5
  for passive DA, 9 otherwise). Single-speed modules take the per-speed step
  once.
- **X552 15ad 10G_T**:
  1. iXFI is forced to 10G, with up to 1 s for LINKS and copper (skipped in
     KR mode).
  2. The X557 advertises 1.0x0004 without 100M, then AN is restarted unless
     vetoed.
  3. At copper link-up, 7.0xC800 gives the speed: iXFI 10G or 1G (KR mode: KR
     AN 10G+1G). 10/100 is reported as not carried.

### Policy choices and deviations, stated

- **No semaphore force-take** (spec 1.4.3, 10 item 15): a held resource
  fails as `Semaphore { held }` and the link is left alone.
  - The X540/X552 start-up clean-up is not run.
  - A REGSMP never granted is not cleared (the spec's "release both"). Only
    SMBI, which this driver took, is given back.
- **Intel-OUI module rule not applied** (spec 4.2 boot-driver note). Unknown
  modules are still refused, and so is 1000BASE-T on the X552.
- **SmartSpeed not used** on 82599 backplanes (spec 9.2 allows it).
- **X552 LASI alarm enables** (spec 7.8.3, optional) are not set: the driver
  polls.
- **#16 (2026-10-06): 154f, the X552 crosstalk fix and QSFP multispeed are
  done.** Each had a defined path in the shared code, added to the spec as
  section 11 (an addendum by this driver's maintainers). An unreadable X552
  crosstalk word is not fatal: the cage check stays off, which is what the
  82599 does with the bit set. None of the three is verified on hardware.
- EERD (NVM word read) is from the 82599 datasheet (8.2.3.2.2). The spec
  assumes NVM access is available.

### Hardware checks (spec section 10)

**Boot verbose for these checks** (#22): most lines they read (`reset (…)`,
`link setup: …`, `SFI firmware patch version`, `multispeed: …`) are trace
lines, printed only with `StormnicVerbose` set before the driver loads, or a
`--features verbose` build (README, "Console output"). The default console
has the one summary line per NIC, which keeps the link state and, on a link
down, the LINKS/AUTOC/AUTOC2/ESDP values.

The X9 blades' Intel ports are 8086:1557, an 82599 SFP+ (server1 at
0000:03:00.0; server3, ac:1f:6b:8a:a4:5c, booted on the rustnic media,
stormbootx#45). server3's 2026-10-01 log (563ea8d, below under item 8)
settles items 1, 2, 4, 5, 6 and 7 for that board: NVM AUTOC c09c6084 kept
as 10G SFI, `laser on`, `link up 10000 Mb/s`, a passive DA multispeed
module, SFI firmware 0x107 and `reset (RST)` (the link was up). Still open
there: item 3 (an empty cage) and the LNK_RST path (item 9, #26). What a log should
settle:

1. **NVM default LMS** (item 10): the `NVM AUTOC` value in the `link setup:
   module …` line. The spec takes capabilities from it (5.6). If a board's NVM
   LMS were 000 with a single-speed 10G module, 5.6 gives 1G. The line shows
   what this board has, and the final AUTOC shows what was set.
2. **SDP3 direction** (item 11): `laser on` versus `laser not driven (SDP3 is
   not an output)`.
3. **Cage presence polarity** (item 12): with `cage-presence check on` in the
   line and a module fitted, the link must still come up. Also boot once with
   the cage empty.
4. **LINKS fields** (item 1): `link up 10000 Mb/s` against Linux on the same
   port.
5. **Module ID timing** (5.14): the module type in the line; an `I2c` error
   or `none` with a module fitted means the I2C path needs a look.
6. **SFI firmware version** (5.12): the `SFI firmware patch version` line
   (expected > 5).
7. **Reset type**: `reset (RST)` or `reset (LNK_RST, link was down)`. The link
   should come up either way.
8. **CFG_DONE** (#21): on server3, expect the `EEMNGCTL CFG_DONE0 not set`
   line straight after the reset line, then the link setup. The BMC (SOL,
   IPMI LAN) should stay reachable across Start.
   **Seen on server3, 2026-10-01** (rustnic media at 563ea8d): `reset (RST),
   LAN 0`, then `EEMNGCTL CFG_DONE0 not set after 1 s (EEMNGCTL 0x80000196)`,
   link setup (passive DA, 10G SFI, SFI firmware 0x107), `link up 10000 Mb/s`,
   `Start: bound, SNP on a child handle`. stormbootx then leased an address
   over the SNP and claimed its boothost. The SOL capture ran without a gap
   across Start.

9. **Link from a link-down start** (#26): power server3 off (so the link
   starts down), boot the rustnic media, and expect `reset (LNK_RST, link was
   down)` and then `link up 10000 Mb/s` (or `multispeed: link at 10000 Mb/s`).
   Only the `RST` (link already up) branch is verified so far.
   **Seen on server3, 2026-10-02 to 10-03** (563ea8d, 10 boots): every boot
   was `LNK_RST`, passive DA identified, NVM sequence and 10G SFI written,
   `laser on`, SFI firmware 0x107, then `link down after 3000 ms`. stormbootx
   leased through the ConnectX-3 instead. The owner puts it down to the Intel
   PHY/cable, fixed when parts arrive (stormbootx#81). A code review of the
   path against spec 5.7, 5.8 and 5.13 found no difference.
   If it still fails after the parts, the `link down` line now carries LINKS,
   AUTOC, AUTOC2 and ESDP, and a multispeed module logs `multispeed: no link
   at 10G or 1G`. Read LINKS against the 82599 datasheet's LINKS table: no
   signal detect and no PCS/lane sync means nothing arrives from the far end
   (cable, module or switch port), while signal with no sync or link points at
   our programming. To compare the cable directly, check the port under
   Linux's ixgbe (`ethtool` on the same port). The `stormbootx-ipxe` media no
   longer exists (stormbootx v0.17.0 dropped iPXE).

Not checkable on the X9 blades (no such hardware known):
- X540: items 3 (PHY MDIO address) and 13 (7.0xC800 decode).
- X552: items 4–5 (NW_MNG_IF_SEL and INT_PHY_MODE on 15ad), 6–7 (IOSF order,
  AN_RESTART self-clear), 8–9 (CS4227 checksum and the scratch handshake over
  AC and warm resets) and 14 (Marvell 1G-T).
- 82599_LS (item 2).

The console lines print what each check needs: the PHY at its MDIO address,
NW_MNG_IF_SEL, LINK_CTRL_1 and the CS4227 reset or not.

### Verification

`sc-build 'scripts/test-hardware.sh && scripts/check-driver.sh'` at
`c174bcc`: 43 tests passed; release image x86_64 PE32+ subsystem 11, 81,408
bytes; exit 0, drive deleted.

The simulated register file now emulates:
- EERD NVM words and ANLP1;
- PHY soft reset;
- a bit-level I2C slave on both I2CCTL layouts: the SFP ID and A2 pages,
  the port expander and the CS4227, whose checksums are checked on every
  transaction, with NACKs recorded.

The tests cover:
- the reset type and its semaphore per family;
- both semaphore algorithms and their bounds;
- the 82599 NVM sequence, the protected write and pipeline reset, LESM, the
  veto, the laser (on, manageability, SDP3 not an output), multispeed with
  hard rate select and the laser flap, 1G SFI, absent/unknown/unlisted
  modules, crosstalk, the backplane, TN1010 and the QSFP handshake;
- the X540 power-on, advertisement and veto;
- X552 KR, KX4/XFI/1G-T, sideband errors, X557 order (unstall, PHY reset, MAC
  reset, iXFI, advertisement), LINKS AND copper, re-forcing once per change,
  KR mode, no PHY, veto;
- the SFP multispeed fallback, the classification order, the CS4227 checksum
  NACK, the pending-peer takeover and a CS4227 that never loads.

None of it has run on hardware.

## Vendor sources

Written from Intel documentation, without translating another driver's code:

- [82599 datasheet, revision 3.5, document 331520](https://cdrdv2-public.intel.com/331520/331520_Intel%C2%AE%2082599%2010%20GbE%20Controller%20Datasheet_rev3_5.pdf):
  sections 4.6.3–4.6.4 (reset and link initialization), 4.6.7.1 (receive queue
  disable), 5.2.5.3.2 (master disable), 8.2.3.1 (CTRL/STATUS), 8.2.3.2 (EEC/
  EEMNGCTL), 8.2.3.7 (RAR), 8.2.3.8 (RXDCTL/RDRXCTL), 8.2.3.22 (LINKS).
- [X540 datasheet, revision 3.1, document 333168](https://cdrdv2-public.intel.com/333168/333168-x540-datasheet-v3-1.pdf):
  sections 3.6.2–3.6.3 (MDIO and PHY initialization), 4.6.3 (reset), 5.2.4.3.2
  (master disable), 8.2.4.1–8.2.4.2 (control/NVM), 8.2.4.7.9–10 (RAR),
  8.2.4.16.7 (LINKS), 11.6.5 (PHY ownership synchronization).
- [Xeon D-1500 datasheet volume 4, document 332053](https://cdrdv2-public.intel.com/332053/xeon-d-1500-datasheet-vol-4.pdf):
  section 4.5.3 (initialization), section 3.8 and appendix B (PHY topologies
  and integrated KR setup). Its X552 SFP+ topology can include an external
  Inphi CS4227 PHY. It cannot use the 82599 AUTOC recipe interchangeably.

## Shared sequence

The caller must own the PCI function and provide BAR0 register access and a
microsecond delay. Reset first rejects active virtualization, masks interrupts,
disables reception and all 128 RX queues, and drains PCIe master requests.
It reads CTRL back before polling STATUS. A drain failure returns without
issuing reset; recovery by forced reset is deliberately not implemented.

Global reset (superseded by #13: now one reset type chosen by the spec,
see above) set software and link reset together. No register access occurs
for the first millisecond; reset completion is bounded to 100 ms, followed by
10 ms settling (now 50 ms, or 100 ms on the X540). Interrupts are masked again. NVM auto-read, the correct LAN's
manageability configuration, and DMA initialization each have a one-second
bound. NVM presence is checked separately because auto-read completion also
occurs when no valid NVM is present.

The manageability configuration wait (EEMNGCTL.CFG_DONE0/1, bit 18 + LAN_ID)
is the one wait that is reported, not fatal (#21). On server3 (X9SRD-F,
8086:1557 shared with the BMC) EEMNGCTL reads `0x80000196` after reset with
neither CFG_DONE bit set, and iPXE and Linux both run that NIC; Linux's
driver also only logs this timeout. EEC.AUTO_RD and EE_PRES have already
confirmed the NVM load by then, so Start logs `EEMNGCTL CFG_DONE<n> not set
after 1 s (EEMNGCTL 0x...); NVM auto-read done, continuing` and carries on.
A removed device or a PCI I/O error during the wait is still fatal.

Manageability sideband (NC-SI to the BMC) is left alone: when the link is up
the reset is CTRL.RST, not LNK_RST, so the link the BMC is using is not
reset; with MMNGC.MNG_VETO set there is no PHY reset, AN restart, AUTOC write
or laser change; and the quiesce only stops host DMA.

RAR0 is read only after the reset and NVM completion. It contains the port's
NVM-provisioned address, not a previous driver's station-address override.
Invalid RAR, zero, broadcast, and multicast addresses are rejected. Locally
administered unicast addresses are accepted. No direct EEPROM writes occur.

The link query uses current LINKS.LINK_UP, ignoring the historical latched
status bit. It reports down, or up at 100/1000/10000 Mb/s; a reserved speed
encoding remains unknown. Removed devices and PCI I/O errors are errors,
never a fabricated link-up indication. Link down is a valid state.

## Verification

Push first, then run:

```sh
sc-build 'scripts/test-hardware.sh && scripts/check-driver.sh'
```

The standalone Rust tests simulate register operations and check ordering,
both LANs, last-queue disable, timeout bounds, invalid NVM/MAC, virtualization,
transport errors, device removal, and current link/speed decoding. They do
not emulate a PHY and cannot prove an electrical link works. The separate
hardware check remains #7; full PHY bring-up and integration are still #2.

Remote verification on 2026-09-28 at `fed1c3b`: all 10 tests passed; the
release driver passed the x86_64 PE32+ subsystem-11 check (25,600 bytes).
The remote job exited 0. The wrapper subsequently reported that its local
`runs.jsonl` was read-only; no host changes were attempted.

## All-variant documentation audit (2026-09-29)

The scope decision is resolved; this is a source-documentation dependency,
not a request to reduce the supported PCI ID list.

The public Xeon D-1500 volume 4 revision 004 identifies external PHYs in
section 9.2.2.2: CS4227 for 15ac, X557-AT2 for 15ad, and Marvell
88E1512/88E1514 for 15ae. Appendix B.5.1 specifies KR/KX advertisement and
restart fields in `KRM_KR_PCS_PORT<n>.LINK_CNTL_1`, but the register's numeric
address was not found in this document. Its reference to section 3.8.1
loops back to Appendix B. Sections 8.2.2.11.1–2 describe the indirect access
transport, not the missing destination register map. Revision history also
removes KX4 support; 15aa needs a source for its older silicon path.

[Inphi reference-design technical note 451265, revision 1.0](https://community.intel.com/cipcp26785/attachments/cipcp26785/processors/43175/1/451265_Intel_CPU_Reference_Design_Procedure_Rev1p0_14Jul2015.pdf)
identifies a CS4227 datasheet and Intel EEPROM Operational Guidelines as
separate vendor-provided documents (page 3). Its example register discussion
is tied to PHY firmware versions and refers to those guidelines for details
(page 5); it is insufficient to implement the complete reset, module-mode
selection, and completion sequence. The full programming documents were not
located in the public-source search. This is not proof that they are unavailable.

Needed to resume the all-variant implementation: a vendor source for the X552
internal PHY register addresses (including the matched older variant), and
CS4227 programming documentation suitable for use in this MIT project.
Do not guess addresses, treat NVM defaults as complete PHY setup, or present
MAC-to-PHY link status as proof of external media link. Once sources are
available, finish auditing X557 and Marvell initialization as well, implement
the distinct PHY paths and PCI lifetime handling, and extend remote tests.
The audit has not established that these are the only remaining source gaps.

No driver code changed during this audit. The feature and version bump remain
pending, and #2 must stay open.

Remote regression verification on 2026-09-29 at `50b6bb5`:
`sc-build 'scripts/test-hardware.sh && scripts/check-driver.sh'` passed all
10 existing tests and the release PE check (x86_64, PE32+, subsystem 11,
25,600 bytes). The remote job exited 0 and its drive was deleted. The local
wrapper again could not append its read-only `runs.jsonl`; no host change
was attempted. This verifies the unchanged scaffold/common primitives only,
not PHY bring-up or a live link. The subsequent commit records these results.

## Memory decode and bus mastering (#19)

Server3 (X9SRD-F, AMI Aptio 4 BIOS 3.0a, 8086:1557) logged `Start could not
enable memory decode and bus mastering: UNSUPPORTED` with the first code,
which read Get and Supported, refused itself when Supported lacked MEMORY or
BUS_MASTER, and then called Enable(MEMORY | BUS_MASTER), all fatal. It never
asked for DUAL_ADDRESS_CYCLE or I/O decode, so the UNSUPPORTED came from one
of those three steps; the old line did not say which. iPXE on the same blade
works.

`src/decode.rs` (UEFI-independent, `test/decode.rs`) now:

1. reads `Get` (to restore) and `Supported`; a failure of either is logged,
   not fatal;
2. calls `Enable` with MEMORY | BUS_MASTER masked by Supported (both when
   Supported failed); if that fails, each bit alone;
3. reads the PCI command register (config 0x04, 16 bits). If Memory Space
   Enable (bit 1) and Bus Master Enable (bit 2) are not both set, it sets
   them with a 16-bit config write (a 32-bit write would clear the status
   register's write-1-to-clear bits) and reads back. Start fails only if
   they are still clear, or config space can't be accessed.

One console line shows every step: `PCI attributes Get G, Supported S,
Enable ...; command C [-> C' (set directly)]`. On server3 (2026-10-01,
8ea722a) it read `Get 0x700, Supported 0x8000000000078763, Enable 0x2:
SUCCESS; command 0x0007`: AMI's Supported has MEMORY but not BUS_MASTER,
which the old code refused, and BME was already set in the command
register. Release undoes only what was done. If `Enable` took
bits, it calls `Set(original)`, or `Disable` of those bits when Get failed.
If the config write was needed, MSE/BME go back to their old values. The
"could not stop DMA" path disables BUS_MASTER and also clears BME in the
command register.

## Start integration and 82599/X540 link setup (2026-09-29, link setup superseded by #13)

(The attribute handling below was replaced by #19, above.)

Start opens PciIo BY_DRIVER, reads the PCI attributes (Get), checks memory
decode is supported, and enables `EFI_PCI_IO_ATTRIBUTE_MEMORY`. Bus mastering
is not enabled: nothing here does DMA (#3 will). The original attributes are
restored with Set on Stop and on any failed Start. Registers are 32-bit
PciIo memory accesses on BAR 0; delays are boot-services Stall.

After `reset` (above), `setup_link` differs per family:

- **82599** — datasheet 4.6.3.2: the link interconnect configuration is set
  through the EEPROM; 4.6.4.1–4.6.4.4: every link type's flow is "EEPROM
  electrical setup, configure AUTOC.LMS / PMA-PMD fields and
  AUTOC2.10G_PMA_PMD_Serial, restart with AUTOC.Restart_AN, verify with
  LINKS". AUTOC and AUTOC2 are loaded from the NVM (8.2.3.22.19/22, fields
  marked `*`), and the NVM is the only statement of the board's media, so
  the driver keeps the loaded values and writes `AUTOC | Restart_AN` once.
  Global reset includes link reset, so the 3.7.4.2 "LMS unchanged" toggle is
  not needed. SW_FW_SYNC's MAC CSR bit is "reserved for future use"
  (10.5.4), so no semaphore is taken for AUTOC. SDP pins (SFP+ TX_DISABLE,
  MOD_ABS) are board-specific — Table 3-13 is an example — and ESDP keeps
  its state across software resets, so ESDP is only read and logged.
- **X540** — datasheet 4.6.2: the NVM holds enough to bring the link up;
  3.6.3.2: the PHY auto-negotiates and software changes its settings only to
  depart from the defaults; 3.6.3.3.3: the PHY is reset with the MAC except
  on software-only reset. No MDIO access is made.
- **X552** — per device; superseded by [X552 PHY
  setup](#x552-phy-setup-2026-09-29) below (at 025a137 it wrote nothing).

`wait_link` then polls current LINKS.LINK_UP every 10 ms for up to 3 s.
Link down at the deadline is reported, not an error.

Known limits, not claimed as done: an SFP+ module whose speed differs from
the NVM link mode (a 1G module on a 10G SFI port) is not detected or
adapted — that needs the SFP I²C module ID, not implemented; link flow
control is not configured (X540 3.6.3.2.2.2, 82599 4.6.3.2 — left zero
until the SNP needs it); link is only sampled in Start, the SNP (#4) now
reports it live (MediaPresent from LINKS in GetStatus). The simulated-register tests check register order and
bounds; they cannot prove an electrical link.

Remote verification at `025a137`: `sc-build 'scripts/test-hardware.sh &&
scripts/check-driver.sh'` — 15 tests passed (5 new: 82599 Restart_AN with
NVM AUTOC preserved, X540/X552 write nothing, 82599 fail-closed on removal
and I/O error, LMS decoding, bounded link wait); release image x86_64 PE32+
subsystem 11, 32,256 bytes. Exit 0, drive deleted.

## X552 PHY setup (2026-09-29, superseded by #13)

The documentation gap above was closed by the owner's answer on #2 ("can you
not look at the C source from linux?"). Linux's ixgbe is GPL, but the same
hardware layer is Intel's *shared code*, which Intel also publishes under
BSD-3-Clause — FreeBSD `sys/dev/ixgbe` (and DPDK `drivers/net/ixgbe/base`).
That is vendor-authored and compatible with this MIT crate, so it is the
source used, read for register addresses, bit fields and the order of
operations. `src/x552.rs` is written here in its own structure (a
register-trait state machine with bounded waits and fail-closed errors, like
`hardware.rs`); it is not a line-by-line port. Files read:
`ixgbe_x550.c` (`ixgbe_init_phy_ops_X550em`, `ixgbe_setup_kr_speed_x550em`,
`ixgbe_setup_ixfi_x550em[_x]`, `ixgbe_setup_mac_link_sfp_x550em`,
`ixgbe_check_cs4227`/`ixgbe_reset_cs4227`, `ixgbe_setup_mux_ctl`,
`ixgbe_init_ext_t_x550em`, `ixgbe_setup_internal_phy_t_x550em`,
IOSF sideband access), `ixgbe_x540.c` (SW_FW_SYNC semaphore), `ixgbe_phy.c`
(MDIO, I2C bit-bang and the CS4227 combined protocol, SFP identification) and
`ixgbe_phy.h`/`ixgbe_type.h` (addresses and fields).

Notice for the source read, as its licence asks:

> Copyright (c) 2001-2020, Intel Corporation. All rights reserved.
> Redistribution and use in source and binary forms, with or without
> modification, are permitted provided that the following conditions are
> met: 1. Redistributions of source code must retain the above copyright
> notice, this list of conditions and the following disclaimer. 2.
> Redistributions in binary form must reproduce the above copyright notice,
> this list of conditions and the following disclaimer in the documentation
> and/or other materials provided with the distribution. 3. Neither the name
> of the Intel Corporation nor the names of its contributors may be used to
> endorse or promote products derived from this software without specific
> prior written permission. THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT
> HOLDERS AND CONTRIBUTORS "AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES,
> INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY
> AND FITNESS FOR A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE
> COPYRIGHT OWNER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT,
> INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT
> LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA,
> OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF
> LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING
> NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
> SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

### Shared mechanisms

- **SW/FW semaphore** (SWSM 0x10140 SMBI, SW_FW_SYNC 0x10160 REGSMP bit 31,
  software bits PHY0/PHY1 1–2 and I2C 11–12, firmware bits 5 and 2 above
  them). Waits: SMBI and REGSMP 2000 × 50 µs, the resource 1000 × 5 ms.
  **Differs from Intel's code:** on timeout it does not take the resource
  from firmware or clear another driver's bits; it fails with
  `Semaphore { held }` (the crate's fail-closed rule).
- **IOSF sideband** to the KR PHY: SB_IOSF_INDIRECT_CTRL 0x11144 (address in
  the low bits, target 0 = KR PHY in 30:28, BUSY bit 31, response status
  19:18) and _DATA 0x11148, under PHY0|PHY1, 100 × 10 µs busy wait. KRM
  registers per port (port 1 = port 0 + 0x4000): LINK_CTRL_1 0x420c
  (force speed 10:8 = 2 for 1G / 4 for 10G, CAP_KX 16, CAP_KR 18, AN_ENABLE
  29, AN_RESTART 31), DSP_TXFFE_STATE_4/5 0x4634/0x4638, RX_TRN_LINKUP_CTRL
  0x4b00, TX_COEFF_CTRL_1 0x5520.
- **MDIO** clause 45 through MSCA 0x425c / MSRWD 0x4260 (address cycle,
  then read or write; 100 × 10 µs each), under the port's PHY semaphore.
- **I2C** bit-banged through I2CCTL 0x15f5c (X550 layout: BB_EN 8, CLK_OUT 9,
  DATA_OUT 10, DATA_OE_N 11, DATA_IN 12, CLK_OE_N 13, CLK_IN 14) at standard
  mode timing, with a nine-clock bus clear and retry on a missing ACK. The
  bus is shared by both ports: taken with PHY0|PHY1|I2C, and port 1 selects
  it through a mux on SDP1 (ESDP) while holding it.

### Per device

| ID | What is written | Console |
|---|---|---|
| 15aa KX4 | nothing: the hardware runs KX4 | `KX4, run by the hardware` |
| 15ab KR | LINK_CTRL_1: AN_ENABLE, CAP_KR+CAP_KX; then AN_RESTART. Skipped if MMNGC.MNG_VETO | `KR PHY auto-negotiating KR+KX` |
| 15ac SFP+ | ESDP mux setup; CS4227 reset once per power-on through port expander 0xe0 bit 1 (500 µs low, 450 ms, EFUSE/EEPROM status polled), recorded in its scratch register (0x5aa5); SFP ID bytes 0, 3, 6, 8 (0x3c, 12, 14, 15 when needed) at 0xa0; KR PHY auto-negotiating only the module's speed; CS4227 LINE_SPARE24_LSB (0x12b0 + port × 0x1000) = EDC mode (CX1 for passive DA, SR otherwise) << 1 \| 1 | `CS4227 …; SFP …, KR PHY 10G/1G, CS4227 EDC CX1/SR` |
| 15ad 10GBASE-T | HLREG0.MDCSPD cleared; X557 found at NW_MNG_IF_SEL's MDIO address or by scanning 0–31; on first start (PMA 0xcc02 bits 1:0) Vendor-1 0xc479 bit 15 (power-up stall) cleared; internal link iXFI forced 10G (training and TX FFE adaptation off, coefficients overridden, AN restart), or KR AN if NW_MNG_IF_SEL.INT_PHY_MODE; then up to 5 s for copper and iXFI re-forced to 1G if copper is 1G | `X557 PHY … internal link Ixfi/Kr`, `copper link up …` |
| 15ae 1000BASE-T | nothing: the external 1G PHY is run by firmware | `external 1G PHY run by firmware` |

### Known limits (not claimed as done)

- No PHY reset or LASI setup on 15ad; the X557's advertisement is its
  provisioning default (Intel's driver rewrites it to the requested speeds).
- 15ac: a multispeed module is run at 10G; SFP+ soft rate select (byte 110
  at 0xa2) is not written. Intel's vendor-OUI enforcement is not applied: any
  module that identifies as a supported type is driven. 1000BASE-T SFPs are
  unsupported (as in Intel's code for this part).
- Link is followed only during Start; a later copper speed change on 15ad
  is not followed. The SNP (#4) reads LINKS in GetStatus but doesn't re-run
  `follow_copper`.
- Flow control is not configured on any family.

### Verification

`sc-build 'scripts/test-hardware.sh && scripts/check-driver.sh'` at
`8407e3c`: 28 tests passed (13 new for X552), release image x86_64 PE32+
subsystem 11, 53,248 bytes; exit 0, drive deleted. The new tests run each
path against a simulated register file: KR writes on the right port and
restart; manageability veto; firmware-held semaphore, never-granted SWSM,
IOSF error and IOSF busy all fail without writes and release what they took;
X557 found by scan or at the NVM address, stall released once, iXFI forced
and re-forced to 1G, bounded waits, no PHY; and the 15ac path against a
bit-level I2C slave model (SFP ID EEPROM, port expander, CS4227 with its
checksummed register protocol checked on every transaction): one CS4227
reset per power-on, passive DA/SR/1G modules, absent and unsupported
modules, and a CS4227 that never loads. None of this has run on an X552.
