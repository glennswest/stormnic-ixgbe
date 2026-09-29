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
binds, 25 device IDs:

| Family | Device IDs |
|---|---|
| 82599 | 10f7, 10f8, 10f9, 10fb (SFP+), 10fc, 1507, 1514, 1517, 151c (10GBASE-T), 1529, 152a, 154a, 154d, 1557, 1558 (QSFP+), 155d (bypass) |
| X540 | 1528 (X540-T), 1560 (X540-T1), 155c (bypass) |
| X552 (Xeon D-1500) | 15aa (KX4), 15ab (KR), 15ac (SFP+), 15ad (X552/X557-AT), 15ae (1000BASE-T), 15b0 (XFI) |

It only binds a function whose class code is network (0x02). Virtual
functions (82599 10ed, 152e; X540 1515, 1530; X552 15a8, 15a9) are
deliberately left out. 82599_LS (154f) is left out too: `docs/spec/phy.md`
section 10 item 2 finds no bring-up path for it. server1's port is
8086:1557 (82599EN SFP+), found by the first boot (#7): the driver logs every
Intel network function it sees, matched or not (#1).

References: Intel 82599 10 GbE Controller Datasheet; Intel Ethernet
Controller X540 Datasheet; Intel X552 (Xeon D) datasheet, for reset and the
NVM MAC. PHY and link programming follows **`docs/spec/phy.md`**, an
independent specification written by an agent that does not write this
driver, from Intel's own shared ixgbe code as Intel publishes it under
BSD-3-Clause (FreeBSD `sys/dev/ixgbe`). `NOTICE` carries Intel's notice; see
the [bring-up notes](docs/bring-up.md#phy-and-link-programming-per-the-spec-13-2026-09-29).

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
80 KB. `build.rs` adds `/SUBSYSTEM:EFI_BOOT_SERVICE_DRIVER` to the link. The
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
  (`src/hardware.rs` and its modules, in the order of `docs/spec/phy.md`
  section 9; see the [bring-up notes](docs/bring-up.md)):
  1. quiesce, and read MMNGC's manageability veto once;
  2. the PHY or module steps the spec puts before the MAC reset:
     - **82599** SFP+: SFP+ module ID over I2C. QSFP+ (1558): the QSFP ID.
       T3 (151c): TN1010 reset.
     - **X552** 15ac: CS4227 reset once per power-on, then module ID.
       15ad: X557 found, its power-up stall released, and the PHY reset.
  3. MAC reset: LNK_RST if the link is down, RST if it is up; always RST
     under the PHY semaphore on the X540. Then NVM auto-read, and the
     port's MAC from RAR0 as the NVM provisioned it;
  4. link setup per family:
     - **82599** SFP+: the NVM's init sequence for the module into CORECTL,
       AUTOC to 10G SFI with a pipeline reset, laser on (SDP3) unless
       manageability owns it, and 10G then 1G for multispeed modules (SDP5
       rate select). Backplane: the NVM advertisement kept. T3: the TN1010
       advertises and restarts AN.
     - **X540**: the PHY powered on, all its speeds advertised, AN restarted.
     - **X552** by device: 15ab KR AN (KR+KX) over the IOSF sideband.
       15ac: the KR PHY and CS4227 EDC mode per module speed (10G then 1G
       with soft rate select for multispeed modules). 15ad: internal iXFI
       forced, X557 advertising 10G+1G. 15aa KX4, 15b0 XFI and 15ae 1G-T:
       nothing written.

     AN restarts, PHY resets and link-mode writes are skipped under the
     manageability veto;
  5. waits for link, polling every 100 ms: 9 s for 10GBASE-T, 3 s otherwise.
     On X552 15ad the internal link is re-forced to the copper speed at
     copper link-up, and link is up only when LINKS and the X557 agree. Link
     down is reported, not an error.

  A PCI I/O error or a removed device fails Start: it restores the
  attributes, releases PciIo and returns DEVICE_ERROR. A PHY or link step
  that fails otherwise is logged and skipped, and Start reports link from
  LINKS alone. Examples: a semaphore firmware holds (never taken from it),
  no PHY, an I2C or sideband error, no NVM init sequence.
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
GetLocation fails. `PHY` is `X540`, `X557`, `TN1010`, `88E1500`, `88E1543`
or `unknown PHY`; `ID` its 32-bit MDIO ID with the revision masked.
`MODULE` is `passive DA`, `active limiting DA`, `10G SR/LR`, `1000BASE-T`,
`1000BASE-SX`, `1000BASE-LX`, `10G BX`, `1000BASE-BX`, `unknown` or `none`,
then `, multispeed` if so and `(id X, 10G X, 1G X, cable X)`. `SPEEDS` is
e.g. `10G+1G+100M`.

| Line | When |
|---|---|
| `stormnic-ixgbe 0.1.0: driver binding installed (25 Intel 10G device IDs)` | entry point, success |
| `stormnic-ixgbe 0.1.0: driver binding not installed: STATUS` | entry point, failure (the image returns that status) |
| `stormnic-ixgbe: LOC 8086:DDDD: Intel network function, not in the 82599/X540/X552 list; not binding` | Supported (and Start), unlisted Intel NIC |
| `stormnic-ixgbe: LOC 8086:DDDD NAME: Supported` | Supported, will bind |
| `stormnic-ixgbe: LOC 8086:DDDD NAME: already driven by another driver (STATUS); leaving it` | Supported, a platform driver owns it |
| `stormnic-ixgbe: LOC 8086:DDDD: manageability veto (MMNGC.MNG_VETO): no PHY reset, AN restart or link-mode write` | Start, manageability owns the link |
| `stormnic-ixgbe: LOC 8086:DDDD: PHY/module check failed: ERROR; link left to hardware, reporting LINKS only` | Start, a step before the reset failed (e.g. `Semaphore { held: .. }`, `NoPhy`, `I2c { .. }`, `Cs4227 { .. }`, `PhyReset`) |
| `stormnic-ixgbe: LOC 8086:DDDD: PHY ID at MDIO N, reset\|not reset (veto)\|not reset (over-temperature alarm)` | Start, 82599 151c (PHY `TN1010`) |
| `stormnic-ixgbe: LOC 8086:DDDD: CS4227 reset\|CS4227 already reset` | Start, X552 15ac |
| `stormnic-ixgbe: LOC 8086:DDDD: PHY ID at MDIO N (NW_MNG_IF_SEL X)[, power-up stall released], reset\|not reset (veto)` | Start, X552 15ad (PHY `X557`) |
| `stormnic-ixgbe: LOC 8086:DDDD: reset (RST\|LNK_RST, link was down), LAN N, MAC xx:xx:xx:xx:xx:xx` | Start, MAC reset done, NVM MAC read |
| `stormnic-ixgbe: LOC 8086:DDDD: link setup failed: ERROR; reporting LINKS only` | Start, link setup failed (e.g. `NoInitSequence { key: .. }`, `PipelineReset`, `Sideband { .. }`, `Semaphore { .. }`) |
| `stormnic-ixgbe: LOC 8086:DDDD: link setup: NVM mode MODE (AUTOC X AUTOC2 X), already as the NVM set it\|advertisement rewritten[, AN complete\|, AN not complete after 4.5 s]` | Start, 82599 backplane/CX4 |
| `stormnic-ixgbe: LOC 8086:DDDD: link setup: module MODULE (AUTOC X AUTOC2 X)` | Start, 82599 SFP+/QSFP+, no module (MODULE `none …`) |
| `stormnic-ixgbe: LOC 8086:DDDD: link setup: module MODULE, not supported; link not set up (AUTOC X AUTOC2 X)` | Start, 82599, unknown module |
| `stormnic-ixgbe: LOC 8086:DDDD: link setup: module MODULE, NVM init sequence N words, MODE (NVM AUTOC X, now AUTOC X AUTOC2 X), laser on\|not driven (SDP3 is not an output)\|left to manageability\|not controlled[, cage-presence check on][, soft rate select failed]` | Start, 82599 module set up |
| `stormnic-ixgbe: LOC 8086:DDDD: SFI firmware patch version 0xN[ (expected > 5)]` | Start, 82599 module set up, NVM has the version |
| `stormnic-ixgbe: LOC 8086:DDDD: multispeed: link at N Mb/s` | Start, 82599 multispeed module linked while trying speeds |
| `stormnic-ixgbe: LOC 8086:DDDD: link setup: PHY ID at MDIO N, advertising SPEEDS, AN and MAC pipeline restarted\|AN not restarted (veto)` | Start, 82599 151c |
| `stormnic-ixgbe: LOC 8086:DDDD: link setup: PHY ID at MDIO N, powered on, advertising SPEEDS, AN restarted\|AN not restarted (veto)` | Start, X540 (PHY `X540`) |
| `stormnic-ixgbe: LOC 8086:DDDD: link setup: KX4, run by the hardware; nothing written` | Start, X552 15aa |
| `stormnic-ixgbe: LOC 8086:DDDD: link setup: XFI, run by the hardware; nothing written` | Start, X552 15b0 |
| `stormnic-ixgbe: LOC 8086:DDDD: link setup: external 1G PHY run by firmware; nothing written` | Start, X552 15ae |
| `stormnic-ixgbe: LOC 8086:DDDD: link setup: KR PHY auto-negotiating KR+KX (LINK_CTRL_1 X), restarted` | Start, X552 15ab |
| `stormnic-ixgbe: LOC 8086:DDDD: link setup: manageability veto (MMNGC.MNG_VETO); link left to firmware` | Start, X552 15ab under the veto |
| `stormnic-ixgbe: LOC 8086:DDDD: link setup: module MODULE[, not supported; link not set up]` | Start, X552 15ac, no module or an unsupported one (unknown, 1000BASE-T) |
| `stormnic-ixgbe: LOC 8086:DDDD: link setup: module MODULE, KR PHY 10G\|1G (LINK_CTRL_1 X), CS4227 EDC CX1\|SR[, multispeed: link at 10G\|1G][, soft rate select failed]` | Start, X552 15ac module set up |
| `stormnic-ixgbe: LOC 8086:DDDD: link setup: internal link iXFI forced\|KR (set at copper link-up), X557 advertising SPEEDS, AN restarted\|AN not restarted (veto)` | Start, X552 15ad |
| `stormnic-ixgbe: LOC 8086:DDDD: copper link up N Mb/s, internal link re-forced N time(s)` | Start, X552 15ad copper up (1000 or 10000) |
| `stormnic-ixgbe: LOC 8086:DDDD: copper link down` | Start, X552 15ad, no copper link within the wait |
| `stormnic-ixgbe: LOC 8086:DDDD: copper link up at a speed the internal link cannot carry (AN vendor status X)` | Start, X552 15ad, copper at 10/100 Mb/s |
| `stormnic-ixgbe: LOC 8086:DDDD: link up N Mb/s` | Start, link up (100, 1000, 2500 on X552, or 10000) |
| `stormnic-ixgbe: LOC 8086:DDDD: link up, speed encoding reserved` | Start, link up with a reserved speed field |
| `stormnic-ixgbe: LOC 8086:DDDD: link down after N ms` | Start, no link (N is 9000 for 10GBASE-T, 3000 otherwise) |
| `stormnic-ixgbe: LOC 8086:DDDD NAME: Start: bound (no SNP yet)` | Start, success |
| `stormnic-ixgbe: LOC 8086:DDDD: Start could not open PciIo BY_DRIVER: STATUS` | Start, failure |
| `stormnic-ixgbe: LOC 8086:DDDD: Start could not enable memory decode: STATUS` | Start, failure |
| `stormnic-ixgbe: LOC 8086:DDDD NAME: bring-up failed: ERROR; releasing` | Start, quiesce/reset/NVM/MAC failed or the device went away (returns DEVICE_ERROR); ERROR is e.g. `Timeout { register: .., .. }`, `MissingNvm`, `InvalidMac`, `Removed`, `Io(..)`, or `Semaphore { .. }` when the reset's PHY semaphore is held |
| `stormnic-ixgbe: could not restore PCI attributes 0xX: STATUS` | Stop or failed Start |
| `stormnic-ixgbe: LOC: Stop: released` | Stop |
| `stormnic-ixgbe: Stop for a controller this driver never started` | Stop, unknown controller (returns DEVICE_ERROR) |

## Status

Scaffold (#1): done; 8086:1557 bound on server1 (#7). Bring-up (#2) with
PHY and link programming matched to `docs/spec/phy.md` (#13): every family
and device path runs in Start, checked in simulation (43 tests, including a
bit-level I2C slave on both I2CCTL layouts for the SFP+ EEPROM, port
expander and CS4227) and not yet on hardware; the checks for server1 are in
the [bring-up notes](docs/bring-up.md#hardware-checks-spec-section-10).
Next: descriptor rings (#3), SNP (#4), then retire `ipxe-intelx.efi` (#5,
stormbootx#27).
