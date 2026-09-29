# Bring-up implementation notes (#2)

Start runs the sequence below through BAR0 (`src/binding.rs` →
`src/hardware.rs`, `src/x552.rs`). The owner requires all 22 matched PCI IDs
(decision recorded on #2 on 2026-09-28). 82599 and X540 link setup are
implemented from their datasheets; X552 PHY setup from Intel's BSD-licensed
shared code, after the owner's answer on #2 (see [X552 PHY
setup](#x552-phy-setup-2026-09-29)). Nothing here has run on hardware yet.

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

Global reset sets software and link reset together. No register access occurs
for the first millisecond; reset completion is bounded to 100 ms, followed by
10 ms settling. Interrupts are masked again. NVM auto-read, the correct LAN's
manageability configuration, and DMA initialization each have a one-second
bound. NVM presence is checked separately because auto-read completion also
occurs when no valid NVM is present.

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

## Start integration and 82599/X540 link setup (2026-09-29)

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
until the SNP needs it); link is only sampled in Start, the SNP (#4) will
report it live. The simulated-register tests check register order and
bounds; they cannot prove an electrical link.

Remote verification at `025a137`: `sc-build 'scripts/test-hardware.sh &&
scripts/check-driver.sh'` — 15 tests passed (5 new: 82599 Restart_AN with
NVM AUTOC preserved, X540/X552 write nothing, 82599 fail-closed on removal
and I/O error, LMS decoding, bounded link wait); release image x86_64 PE32+
subsystem 11, 32,256 bytes. Exit 0, drive deleted.

## X552 PHY setup (2026-09-29)

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
  needs the SNP (#4) to re-run `follow_copper`.
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
