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
  - **In progress (#2), 2026-09-28:** Read Intel register/reset/PHY documentation; implement bounded waits and error propagation through PciIo, preserve board-specific configuration, integrate Start/Stop, and add host-side simulated-register tests. Push before running tests and the PE check with sc-build. Hardware verification remains #7; do not block the code chain on that boot. Record any unsupported PHY requirements explicitly rather than guessing register sequences.
- [ ] Descriptor rings: legacy or advanced RX/TX descriptors, DMA buffers via `EFI_PCI_IO_PROTOCOL` Map/Unmap
- [ ] `EFI_SIMPLE_NETWORK_PROTOCOL`: Start/Stop/Initialize/Reset/Shutdown, ReceiveFilters (unicast, broadcast, multicast), Transmit/Receive, GetStatus, StationAddress; install on a child handle with a MAC device path
- [ ] Test on server1 (X9 blade): only this driver in `\stormboot\drivers`, stormbootx prints `tcp4 : available` and claims its boothost
- [ ] Retire `ipxe-intelx.efi` from the stormbootx media (stormbootx#27)
