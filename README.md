# stormnic-ixgbe

A UEFI driver, in Rust (`no_std`), that gives firmware an
`EFI_SIMPLE_NETWORK_PROTOCOL` for **Intel 82599 / X540 / X552 10 Gb Ethernet (the Linux `ixgbe` family)**.

## Why it exists

stormbootx boots a machine over NVMe/TCP using the firmware's own TCP/IP
stack (`EFI_TCP4`). That stack needs a NIC driver underneath it. Some
platforms have the stack but no UEFI driver for their NIC: the Supermicro X9
blades (server1–8) have only legacy option ROMs for their Intel 10G and
ConnectX-3 ports, so no network handle exists and `EFI_TCP4` never appears
(stormbootx#26).

stormbootx loads every `*.efi` in `\stormboot\drivers` on its boot media
before it looks for TCP4. This driver is one of them. The firmware's MNP, IP4
and TCP4 drivers bind on top of the SNP it installs. There is no PXE, no DHCP
boot and no network code of its own above the link layer.

The interim driver is iPXE's `ipxe-intelx.efi` (GPL-2 C, built from pinned
source). This crate replaces it (stormbootx#27).

## Hardware

- PCI IDs: 8086:10fb (82599 SFP+), 8086:1528 (X540-T2), 8086:15ad/15ab (X552) — confirm the blades' exact ID with `lspci -nn` or the EFI shell `pci` command
- References: Intel 82599 10 GbE Controller Datasheet; Intel Ethernet Controller X540 Datasheet; Intel X552 (Xeon D) datasheet

**Written from the vendor documentation, not translated from iPXE or Linux.**
Reading other drivers for behaviour is fine; copying their code or structure
would make this a GPL derivative, and it is MIT.

## Build

`x86_64-unknown-uefi`, built on dev with `sc-build` after pushing:

```bash
sc-build 'cargo build --release --target x86_64-unknown-uefi'
```

## What it does today

The image is linked as an EFI boot-service driver (`build.rs`). Its entry
point installs `EFI_DRIVER_BINDING_PROTOCOL` on its image handle and prints

```
stormnic-ixgbe 0.1.0: driver binding installed (22 Intel 10G device IDs)
```

When the firmware connects controllers (stormbootx does, after loading every
driver on its media):

- **Supported** reads each PCI function's vendor/device ID through
  `EFI_PCI_IO_PROTOCOL` and accepts the Intel 82599/X540/X552 IDs in
  `src/ids.rs`. If a platform driver already has the NIC's PciIo open
  `BY_DRIVER`, it prints `already driven by another driver; leaving it` and
  declines, so the platform's driver wins.
- **Start** holds PciIo `BY_DRIVER` and prints `Start: bound`. **Stop**
  releases it.
- An Intel network function whose ID is not in the list is printed with its
  ID (`not in the 82599/X540/X552 list`), so a boot names the device a
  machine really has.

There is no bring-up or SNP yet (see CLAUDE.md), so a bound NIC has no
network handle. **Until the SNP lands, don't put this driver on media next to
`ipxe-intelx.efi`:** whichever binds first holds the NIC, and if it's this one
the NIC has no SNP.

To build it and check the image is a boot-service driver (PE subsystem 11):

```bash
sc-build scripts/check-driver.sh
```
