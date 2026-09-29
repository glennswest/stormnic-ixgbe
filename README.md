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

`src/ids.rs` lists the Intel (8086) 10 GbE physical functions the driver
binds, 22 device IDs:

| Family | Device IDs |
|---|---|
| 82599 | 10f7, 10f8, 10f9, 10fb (SFP+), 10fc, 1507, 1514, 1517, 151c (10GBASE-T), 1529, 152a, 154a, 154d, 1557, 1558 |
| X540 | 1528 (X540-T), 1560 (X540-T1) |
| X552 (Xeon D-1500) | 15aa, 15ab, 15ac (SFP+), 15ad (X552/X557-AT), 15ae (1000BASE-T) |

It only binds a function whose class code is network (0x02). Virtual functions (82599
10ed, X540 1515, X552 15a8) are deliberately left out. server1's port is
8086:1557 (82599EN SFP+), found by the first boot (#7): the driver logs every
Intel network function it sees, matched or not (#1).

References: Intel 82599 10 GbE Controller Datasheet; Intel Ethernet
Controller X540 Datasheet; Intel X552 (Xeon D) datasheet.

**Written from the vendor documentation, not translated from iPXE or Linux.**
Reading other drivers for behaviour is fine; copying their code or structure
would make this a GPL derivative, and it is MIT.

## Build

Target `x86_64-unknown-uefi`, built on dev with `sc-build` after pushing.
`scripts/check-driver.sh` builds the release image and checks it is an x86_64
PE32+ with subsystem 11 (EFI boot-service driver), not 10 (application). The
firmware unloads an application as soon as its entry point returns.

```bash
sc-build scripts/check-driver.sh
# or just the build:
sc-build 'cargo build --release --target x86_64-unknown-uefi'
```

`Cargo.lock` is committed (#11) and `scripts/check-driver.sh` builds with
`--locked`, so a pinned commit always builds with the same dependency
versions (`uefi` 0.39.0, `uefi-raw` 0.15.1). Updating a dependency is a
deliberate change to `Cargo.lock` in its own commit.

The image is `target/x86_64-unknown-uefi/release/stormnic-ixgbe.efi`, about
25 KB. `build.rs` adds `/SUBSYSTEM:EFI_BOOT_SERVICE_DRIVER` to the link. The
release profile is size-optimised (`opt-level = "z"`, LTO, `panic = "abort"`,
stripped).

There are no configuration keys, options, files or ports. The driver takes
no input apart from the PCI functions the firmware offers it.

## How it ships

The driver is a file in `\stormboot\drivers` on the stormbootx boot media.
It is not a stormcentral component and has no golden. sc-build keeps nothing
from a build. So stormbootx has to build the `.efi` from a pinned commit, the
way its `scripts/build-nic-drivers.sh` builds iPXE's drivers. Build it with
`cargo build --locked --release --target x86_64-unknown-uefi` from any
commit at or after the one that added `Cargo.lock` (#11). That isn't done
yet: **stormbootx#29**. Until it lands, the driver can't reach any media.

## What it does today

The entry point installs `EFI_DRIVER_BINDING_PROTOCOL` on the image handle
(through the `uefi` crate's `driver` module) and returns. The image stays
resident. The firmware's `ConnectController`, which stormbootx runs after loading every
driver on its media, then calls the binding for each controller:

- **Supported** opens `EFI_PCI_IO_PROTOCOL` with GET_PROTOCOL and reads
  the vendor/device ID and the class code from config space. For one of the IDs
  above, it then tries a BY_DRIVER open. If another driver already
  holds the function's PciIo BY_DRIVER (a platform driver that owns the NIC),
  that open fails and the driver declines, so the platform's driver wins.
  Handles without PciIo, and non-Intel or non-network functions, are
  declined silently.
- **Start** keeps PciIo open BY_DRIVER, enables memory decode (saving the
  PCI attributes it found) and brings the NIC up through BAR0
  (`src/hardware.rs`, see [bring-up notes](docs/bring-up.md)):
  1. quiesce and global reset, NVM auto-read, and the port's MAC from RAR0
     as the NVM provisioned it;
  2. link setup per family: **82599** applies the link mode its NVM loaded
     into AUTOC/AUTOC2 with Restart_AN (SDP pins such as SFP+ TX_DISABLE are
     board-specific and left as firmware set them); **X540**'s integrated PHY
     negotiates from its NVM image, nothing is written; **X552** PHY setup is
     not implemented yet (vendor documentation pending, #2), so it runs on
     NVM defaults and says so;
  3. waits up to 3 s for link; link down is reported, not an error.

  If any step fails, Start restores the attributes, releases PciIo and
  returns DEVICE_ERROR.
- **Stop** restores the PCI attributes and drops the PciIo open.

`EFI_PCI_IO_PROTOCOL` is defined in `src/pci_io.rs` from the UEFI spec
(§14.4), because the `uefi` crate doesn't have it. The driver uses config
reads, GetLocation, Attributes and 32-bit memory reads/writes on BAR0.

`src/hardware.rs` is independent of UEFI and has a standalone
simulated-register test harness (`sc-build scripts/test-hardware.sh`).

There is **no DMA and no SNP yet** (#3, #4), so a bound NIC
has no network handle. **Until #4 lands, don't put this driver on media next
to `ipxe-intelx.efi`:** whichever driver binds first holds the NIC, and if
it's this one, the NIC has no SNP.

### Console output

Everything goes to the console, which on the blades is the SOL capture on
stormcentral. `LOC` is `seg:bus:dev.fn`, or `(location unknown)` if
GetLocation fails.

| Line | When |
|---|---|
| `stormnic-ixgbe 0.1.0: driver binding installed (22 Intel 10G device IDs)` | entry point, success |
| `stormnic-ixgbe 0.1.0: driver binding not installed: STATUS` | entry point, failure (the image returns that status) |
| `stormnic-ixgbe: LOC 8086:DDDD: Intel network function, not in the 82599/X540/X552 list; not binding` | Supported (and Start), unlisted Intel NIC |
| `stormnic-ixgbe: LOC 8086:DDDD NAME: Supported` | Supported, will bind |
| `stormnic-ixgbe: LOC 8086:DDDD NAME: already driven by another driver (STATUS); leaving it` | Supported, a platform driver owns it |
| `stormnic-ixgbe: LOC 8086:DDDD: reset, LAN N, MAC xx:xx:xx:xx:xx:xx` | Start, reset done, NVM MAC read |
| `stormnic-ixgbe: LOC 8086:DDDD: link setup: NVM mode MODE (AUTOC X AUTOC2 X ESDP X), restarted` | Start, 82599 (MODE e.g. `10G SFI`, `KX/KX4/KR AN`) |
| `stormnic-ixgbe: LOC 8086:DDDD: link setup: integrated PHY auto-negotiates from its NVM image` | Start, X540 |
| `stormnic-ixgbe: LOC 8086:DDDD: link setup: X552 PHY setup not implemented (vendor documentation pending, #2); NVM defaults only` | Start, X552 |
| `stormnic-ixgbe: LOC 8086:DDDD: link up N Mb/s` | Start, link up (100, 1000 or 10000) |
| `stormnic-ixgbe: LOC 8086:DDDD: link up, speed encoding reserved` | Start, link up with a reserved speed field |
| `stormnic-ixgbe: LOC 8086:DDDD: link down after 3000 ms` | Start, no link (no cable, or partner still negotiating) |
| `stormnic-ixgbe: LOC 8086:DDDD NAME: Start: bound (no SNP yet)` | Start, success |
| `stormnic-ixgbe: LOC 8086:DDDD: Start could not open PciIo BY_DRIVER: STATUS` | Start, failure |
| `stormnic-ixgbe: LOC 8086:DDDD: Start could not enable memory decode: STATUS` | Start, failure |
| `stormnic-ixgbe: LOC 8086:DDDD NAME: bring-up failed: ERROR; releasing` | Start, reset/NVM/MAC/link step failed (returns DEVICE_ERROR); ERROR is e.g. `Timeout { register: .., .. }`, `MissingNvm`, `InvalidMac`, `Removed` |
| `stormnic-ixgbe: could not restore PCI attributes 0xX: STATUS` | Stop or failed Start |
| `stormnic-ixgbe: LOC: Stop: released` | Stop |
| `stormnic-ixgbe: Stop for a controller this driver never started` | Stop, unknown controller (returns DEVICE_ERROR) |

## Status

Scaffold (#1): done; 8086:1557 bound on server1 (#7). Bring-up (#2): reset,
NVM MAC, 82599/X540 link setup and link status run in Start, checked in
simulation and not yet on hardware; X552 PHY setup waits on vendor
documentation (owner question on #2). Next: descriptor rings (#3), SNP (#4), then
retire `ipxe-intelx.efi` (#5, stormbootx#27).
