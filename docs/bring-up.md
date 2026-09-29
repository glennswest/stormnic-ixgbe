# Bring-up implementation notes (#2)

The shared primitives in `src/hardware.rs` are compiled into the UEFI source
module tree, but Start does not call them yet. The owner requires all 22 matched
PCI IDs (decision recorded on #2 on 2026-09-28). Link setup and binding
integration remain unfinished; the documentation gap below needs owner input.
No new hardware support is claimed by this checkpoint.

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
