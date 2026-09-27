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
- A platform's own driver must win: if the NIC already has an SNP, do nothing.
- Everything the driver does is logged to the console. The only way to debug
  it on the blades is the SOL capture on stormcentral
  (`/var/lib/stormcentral/console/serverN/sol.log`).

## Build

```bash
sc-build 'cargo build --release --target x86_64-unknown-uefi'
```

## Test

Put the built `.efi` in a stormbootx ISO's `\stormboot\drivers`, **without**
`ipxe-intelx.efi` (`scripts/build-boot-agent.sh --iso --drivers DIR` in
stormbootx). Boot server1 from the virtual CD, which the stormcentral minismbd
serves as `\boot\stormbootx.iso`. Ask the master to swap the ISO and boot
the blade; the master holds the BMC and console access.

## Version

`Cargo.toml` → `package.version`. Current: `v0.1.0`.

## Work plan

- [ ] Driver scaffold: `EFI_DRIVER_BINDING_PROTOCOL` (Supported/Start/Stop), matching the PCI IDs; build as an EFI boot-service driver (`/subsystem:efi_boot_service_driver`)
- [ ] Bring-up from the datasheet: reset, EEPROM/MAC read, link setup (SFP+ and 10GBASE-T), link status
- [ ] Descriptor rings: legacy or advanced RX/TX descriptors, DMA buffers via `EFI_PCI_IO_PROTOCOL` Map/Unmap
- [ ] `EFI_SIMPLE_NETWORK_PROTOCOL`: Start/Stop/Initialize/Reset/Shutdown, ReceiveFilters (unicast, broadcast, multicast), Transmit/Receive, GetStatus, StationAddress; install on a child handle with a MAC device path
- [ ] Test on server1 (X9 blade): only this driver in `\stormboot\drivers`, stormbootx prints `tcp4 : available` and claims its boothost
- [ ] Retire `ipxe-intelx.efi` from the stormbootx media (stormbootx#27)
