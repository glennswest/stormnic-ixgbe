# Bring-up implementation notes (#2)

The shared primitives in `src/hardware.rs` are compiled into the UEFI source
module tree, but Start does not call them yet. The PHY scope decision and link
setup must be resolved before binding integration. No new hardware support is
claimed by this checkpoint.

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
