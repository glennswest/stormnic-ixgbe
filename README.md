# stormnic-ixgbe

A UEFI driver, in Rust (`no_std`), that gives firmware an
`EFI_SIMPLE_NETWORK_PROTOCOL` for **Intel 82599 / X540 / X552 10 Gb Ethernet (the Linux `ixgbe` family)**.

## Why it exists

stormbootx boots a machine over NVMe/TCP. Since stormbootx v0.9.0
(stormbootx#56) it carries its own TCP/IP stack, smoltcp, and runs it
directly on each NIC's `EFI_SIMPLE_NETWORK_PROTOCOL`, which it opens
`EXCLUSIVE` (a firmware MNP bound to it lets go). It no longer uses the
firmware's `EFI_TCP4`. Either way it needs a NIC driver that provides an SNP.
Some platforms have none: the Supermicro X9 blades (server1–8) have only
legacy option ROMs for their Intel 10G and ConnectX-3 ports, so no network
handle exists (stormbootx#26).

stormbootx loads every `*.efi` in `\stormboot\drivers` on its boot media
and connects controllers before it looks for SNPs. This driver is one of
them. On success stormbootx prints `tcp4 : smoltcp over SNP (nic N MAC)`.
The driver has no PXE, no DHCP and no network code of its own above the
link layer.

It replaced the interim iPXE driver, `ipxe-intelx.efi` (GPL-2 C). stormbootx
v0.17.0 (2026-10-06, stormbootx#91) builds no iPXE at all, so this crate is
the only Intel 10G driver stormbootx ships (#5, stormbootx#27, stormbootx#81).

## Hardware

`src/ids.rs` lists the Intel (8086) 10 GbE physical functions the driver
binds, 26 device IDs:

| Family | Device IDs |
|---|---|
| 82599 | 10f7, 10f8, 10f9, 10fb (SFP+), 10fc, 1507, 1514, 1517, 151c (10GBASE-T), 1529, 152a, 154a, 154d, 154f (LS), 1557, 1558 (QSFP+), 155d (bypass) |
| X540 (simulation only) | 1528 (X540-T), 1560 (X540-T1), 155c (bypass) |
| X552 (Xeon D-1500; simulation only) | 15aa (KX4), 15ab (KR), 15ac (SFP+), 15ad (X552/X557-AT), 15ae (1000BASE-T), 15b0 (XFI) |

The X540 and X552 paths have run only against the simulated devices in
`test/hardware.rs`; no lab machine has those chips. The owner chose to ship
them anyway (#15), and Start prints a warning when one runs (#17).

It only binds a function whose class code is network (0x02). Virtual
functions (82599 10ed, 152e; X540 1515, 1530; X552 15a8, 15a9) are
deliberately left out. 82599_LS (154f) is missing from Intel's shared-code
MAC-type table, but its media type gives it the backplane path (the NVM's
AUTOC; `docs/spec/phy.md` 11.1, #16); it has not been seen on hardware.
server1's port is
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
100 KB. `build.rs` adds `/SUBSYSTEM:EFI_BOOT_SERVICE_DRIVER` to the link. The
release profile is size-optimised (`opt-level = "z"`, LTO, `panic = "abort"`,
stripped).

There are no configuration keys, files or ports. The driver takes no input
apart from the PCI functions the firmware offers it and the `StormnicVerbose`
EFI variable (console output only, see "Console output"). The `verbose`
Cargo feature turns the full trace on at build time:
`sc-build 'cargo build --release --locked --target x86_64-unknown-uefi --features verbose'`.

## How it ships

The driver is a file in `\stormboot\drivers` on the stormbootx boot media.
It is not a stormcentral component and has no golden of its own. stormbootx
builds it from a pinned commit (stormbootx#29, done 2026-09-28):
`scripts/build-nic-drivers.sh` there fetches this repo at
`STORMNIC_IXGBE_REF` and runs `cargo build --locked --release --target
x86_64-unknown-uefi` (stormbootx#43), checks subsystem 11, and records the
commit and digest in `STORMNIC-SOURCE.txt`. At this writing the pin is
563ea8d.

Since stormbootx v0.17.0 (stormbootx#52, #91; the owner's answer on
stormbootx#81: no iPXE) there are two media and no iPXE anywhere:
- **`nic-drivers` golden:** `bin/stormnic-ixgbe.efi` and
  `bin/stormnic-mlx4.efi`, both loadable, with `STORMNIC-SOURCE.txt`. Its
  `build-golden.sh` refuses any `ipxe-*` file.
- **`stormbootx-rustnic` media:** carries both drivers in `\stormboot\drivers`
  and loads them. Its console shows `media : rustnic ixgbe@<sha>
  mlx4@<sha>`. This is the medium for the X9 blades, whose firmware has no
  driver for the 82599.
- **`stormbootx` media:** the firmware-drivers medium (`media : fw`). It
  carries no NIC drivers, for machines whose firmware drives its own NICs.

A new driver commit reaches a blade by asking stormbootx (an issue there) to
move `STORMNIC_IXGBE_REF` and rebuild the rustnic golden; the master boots
the blade from it.

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
- **Start** keeps PciIo open BY_DRIVER, enables memory decode and bus
  mastering ([how](docs/bring-up.md#memory-decode-and-bus-mastering-19):
  PciIo `Attributes` where the firmware takes them, the PCI command
  register where it refuses, as AMI Aptio 4 does) and brings the NIC up through BAR0
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
       rate select; none on QSFP+). Backplane and 154f: the NVM
       advertisement kept. T3: the TN1010 advertises and restarts AN.
     - **X540**: the PHY powered on, all its speeds advertised, AN restarted.
     - **X552** by device: 15ab KR AN (KR+KX) over the IOSF sideband.
       15ac: the NVM's crosstalk-fix word read through the firmware host
       interface, then the KR PHY and CS4227 EDC mode per module speed (10G
       then 1G with soft rate select for multispeed modules). 15ad: internal iXFI
       forced, X557 advertising 10G+1G. 15aa KX4, 15b0 XFI and 15ae 1G-T:
       nothing written.

     AN restarts, PHY resets and link-mode writes are skipped under the
     manageability veto;
  5. waits for link, polling every 100 ms: 9 s for 10GBASE-T, 3 s otherwise.
     On X552 15ad the internal link is re-forced to the copper speed at
     copper link-up, and link is up only when LINKS and the X557 agree.
     With the NVM's crosstalk fix on (82599 SFP+/QSFP+, X552 15ac) an empty
     cage is link down. Link down is reported, not an error;
  6. maps the descriptor rings and buffers for DMA, starts RX/TX queue 0,
     and, if the link is up, runs the DMA check: one broadcast frame sent,
     up to 3 s listening for any frame, GPTC/GPRC logged. Then it stops the
     queues again, so nothing DMAs until the SNP is initialized. See
     [descriptor rings and DMA](docs/rings.md);
  7. installs `EFI_SIMPLE_NETWORK_PROTOCOL` and a device path (the
     controller's, plus a MAC address node) on a **child handle**, which
     opens the controller's PciIo BY_CHILD_CONTROLLER. stormbootx opens the
     child's SNP `EXCLUSIVE` and drives it with smoltcp (on other firmware
     paths the firmware's MNP may bind to it); the SNP's Initialize starts
     the queues. An
     ExitBootServices event stops them again. See
     [Simple Network Protocol](docs/snp.md).

  A PCI I/O error or a removed device fails Start: it undoes its PCI
  attribute and command-register changes, releases PciIo and returns DEVICE_ERROR. A PHY or link step
  that fails otherwise is logged and skipped, and Start reports link from
  LINKS alone. Examples: a semaphore firmware holds (never taken from it),
  no PHY, an I2C or sideband error, no NVM init sequence.
- **Stop** with the child uninstalls the SNP and device path from it (and
  fails, keeping it, while a consumer such as stormbootx still has the SNP
  open). Stop without children
  stops the queues, unmaps and frees the DMA region, undoes its PCI
  attribute and command-register changes and drops the PciIo open. If the queues can't be stopped,
  bus mastering is disabled and the region is left allocated (never freed
  under a NIC that might still write to it).

`EFI_PCI_IO_PROTOCOL` is defined in `src/pci_io.rs` from the UEFI spec
(§14.4), because the `uefi` crate doesn't have it. The driver uses config
reads, GetLocation, Attributes, 32-bit memory reads/writes on BAR0, and
AllocateBuffer/Map/Unmap/FreeBuffer for the DMA region.

`src/hardware.rs` (with `src/rings.rs`) is independent of UEFI and has
standalone simulated-device test harnesses (`test/hardware.rs`,
`test/rings.rs`, `test/snp.rs` with the DMA-capable NIC in `test/sim.rs`;
`sc-build scripts/test-hardware.sh`). `src/snp_core.rs` (`hardware::snp`) is
the SNP's UEFI-independent state machine.

The driver installs its own `EFI_DRIVER_BINDING_PROTOCOL` (`src/binding.rs`)
rather than the `uefi` crate's `driver::install`, which refuses Stop with
children.

No other Intel 10G driver is on any stormbootx medium (iPXE's
`ipxe-intelx.efi` was retired in stormbootx v0.17.0, #5). If one were added,
whichever binds a NIC first would hold it, depending on load order. A
platform's own driver still wins: Supported declines a NIC that another
driver already holds.

### Console output

Everything goes to the console, which on the blades is the SOL capture on
stormcentral. By default the driver prints **one line per NIC** it binds
(PCI address, MAC, link, `SNP installed`), plus every warning and error
(#22). The rest of the bring-up trace is printed only in verbose mode
(below); otherwise the last 16 trace lines of the current Start are kept, and
a failure prints them first, then the failure. The Default column says which
lines are printed without verbose.

**Verbose** is on when either:
- the driver is built with `--features verbose`; or
- the EFI variable `StormnicVerbose`, vendor GUID
  `ce1479a2-eab9-4176-b0ad-c909ea5b8e0b`, exists and its first byte is not 0.
  The driver reads it once, at its entry point, so it must be set before the
  driver is loaded. The GUID is meant for all the stormnic drivers. From the
  UEFI shell, volatile (gone at the next reset):
  `setvar StormnicVerbose -guid ce1479a2-eab9-4176-b0ad-c909ea5b8e0b -bs =01`.
  stormbootx setting it from its own config is stormbootx#102.

A quiet boot on server3 prints, for the 82599, the one line
`stormnic-ixgbe 0.1.0: 0000:03:00.0 8086:1557 82599EN SFP+: MAC ac:1f:6b:8a:a4:5c, link up 10000 Mb/s, SNP installed`.

`LOC` is `seg:bus:dev.fn`, or `(location unknown)` if
GetLocation fails. `PHY` is `X540`, `X557`, `TN1010`, `88E1500`, `88E1543`
or `unknown PHY`; `ID` its 32-bit MDIO ID with the revision masked.
`MODULE` is `passive DA`, `active limiting DA`, `10G SR/LR`, `1000BASE-T`,
`1000BASE-SX`, `1000BASE-LX`, `10G BX`, `1000BASE-BX`, `unknown` or `none`,
then `, multispeed` if so and `(id X, 10G X, 1G X, cable X)`. `SPEEDS` is
e.g. `10G+1G+100M`.

| Line | When | Default |
|---|---|---|
| `stormnic-ixgbe 0.1.0: driver binding installed (26 Intel 10G device IDs)` | entry point, success | verbose |
| `stormnic-ixgbe 0.1.0: driver binding not installed: STATUS` | entry point, failure (the image returns that status) | always |
| `stormnic-ixgbe: LOC 8086:DDDD: Intel network function, not in the 82599/X540/X552 list; not binding` | Supported (and Start), unlisted Intel NIC | verbose |
| `stormnic-ixgbe: LOC 8086:DDDD NAME: Supported` | Supported, will bind | verbose |
| `stormnic-ixgbe: LOC 8086:DDDD NAME: already driven by another driver (STATUS); leaving it` | Supported, a platform driver owns it | always |
| `stormnic-ixgbe: LOC 8086:DDDD: X540/X552 path: verified in simulation only` | Start, an X540 or X552 ID: the first bring-up line, once per Start (#15, #17) | always |
| `stormnic-ixgbe: LOC 8086:DDDD: manageability veto (MMNGC.MNG_VETO): no PHY reset, AN restart or link-mode write` | Start, manageability owns the link | always |
| `stormnic-ixgbe: LOC 8086:DDDD: PHY/module check failed: ERROR; link left to hardware, reporting LINKS only` | Start, a step before the reset failed (e.g. `Semaphore { held: .. }`, `NoPhy`, `I2c { .. }`, `Cs4227 { .. }`, `PhyReset`) | always, after the kept steps |
| `stormnic-ixgbe: LOC 8086:DDDD: PHY ID at MDIO N, reset\|not reset (veto)\|not reset (over-temperature alarm)` | Start, 82599 151c (PHY `TN1010`) | always with the over-temperature alarm, else verbose |
| `stormnic-ixgbe: LOC 8086:DDDD: CS4227 reset\|CS4227 already reset` | Start, X552 15ac | verbose |
| `stormnic-ixgbe: LOC 8086:DDDD: PHY ID at MDIO N (NW_MNG_IF_SEL X)[, power-up stall released], reset\|not reset (veto)` | Start, X552 15ad (PHY `X557`) | verbose |
| `stormnic-ixgbe: LOC 8086:DDDD: reset (RST\|LNK_RST, link was down), LAN N, MAC xx:xx:xx:xx:xx:xx` | Start, MAC reset done, NVM MAC read | verbose |
| `stormnic-ixgbe: LOC 8086:DDDD: EEMNGCTL CFG_DONEn not set after 1 s (EEMNGCTL 0xXXXXXXXX); NVM auto-read done, continuing` | Start, after the reset: this port's configuration-done bit never set (seen on the X9 blades, #21); not fatal | verbose |
| `stormnic-ixgbe: LOC 8086:DDDD: link setup failed: ERROR; reporting LINKS only` | Start, link setup failed (e.g. `NoInitSequence { key: .. }`, `PipelineReset`, `Sideband { .. }`, `Semaphore { .. }`) | always, after the kept steps |
| `stormnic-ixgbe: LOC 8086:DDDD: link setup: NVM mode MODE (AUTOC X AUTOC2 X), already as the NVM set it\|advertisement rewritten[, AN complete\|, AN not complete after 4.5 s]` | Start, 82599 backplane/CX4/LS (154f) | verbose |
| `stormnic-ixgbe: LOC 8086:DDDD: link setup: module MODULE (AUTOC X AUTOC2 X)` | Start, 82599 SFP+/QSFP+, no module (MODULE `none …`) | verbose |
| `stormnic-ixgbe: LOC 8086:DDDD: link setup: module MODULE, not supported; link not set up (AUTOC X AUTOC2 X)` | Start, 82599, unknown module | always |
| `stormnic-ixgbe: LOC 8086:DDDD: link setup: module MODULE, NVM init sequence N words, MODE (NVM AUTOC X, now AUTOC X AUTOC2 X), laser on\|not driven (SDP3 is not an output)\|left to manageability\|not controlled[, cage-presence check on][, soft rate select failed]` | Start, 82599 module set up | always with `soft rate select failed`, else verbose |
| `stormnic-ixgbe: LOC 8086:DDDD: SFI firmware patch version 0xN[ (expected > 5)]` | Start, 82599 module set up, NVM has the version | always when ≤ 5, else verbose |
| `stormnic-ixgbe: LOC 8086:DDDD: multispeed: link at N Mb/s` | Start, 82599 multispeed module linked while trying speeds | verbose |
| `stormnic-ixgbe: LOC 8086:DDDD: multispeed: no link at 10G or 1G; left at 10G` | Start, 82599 multispeed module: 10G, 1G and 10G again all stayed down (#26) | verbose |
| `stormnic-ixgbe: LOC 8086:DDDD: link setup: PHY ID at MDIO N, advertising SPEEDS, AN and MAC pipeline restarted\|AN not restarted (veto)` | Start, 82599 151c | verbose |
| `stormnic-ixgbe: LOC 8086:DDDD: link setup: PHY ID at MDIO N, powered on, advertising SPEEDS, AN restarted\|AN not restarted (veto)` | Start, X540 (PHY `X540`) | verbose |
| `stormnic-ixgbe: LOC 8086:DDDD: link setup: KX4, run by the hardware; nothing written` | Start, X552 15aa | verbose |
| `stormnic-ixgbe: LOC 8086:DDDD: link setup: XFI, run by the hardware; nothing written` | Start, X552 15b0 | verbose |
| `stormnic-ixgbe: LOC 8086:DDDD: link setup: external 1G PHY run by firmware; nothing written` | Start, X552 15ae | verbose |
| `stormnic-ixgbe: LOC 8086:DDDD: link setup: KR PHY auto-negotiating KR+KX (LINK_CTRL_1 X), restarted` | Start, X552 15ab | verbose |
| `stormnic-ixgbe: LOC 8086:DDDD: link setup: manageability veto (MMNGC.MNG_VETO); link left to firmware` | Start, X552 15ab under the veto | verbose (the veto line above is always shown) |
| `stormnic-ixgbe: LOC 8086:DDDD: link setup: module MODULE[, not supported; link not set up]` | Start, X552 15ac, no module or an unsupported one (unknown, 1000BASE-T) | always when a module is fitted but not supported, else verbose |
| `stormnic-ixgbe: LOC 8086:DDDD: link setup: module MODULE, KR PHY 10G\|1G (LINK_CTRL_1 X), CS4227 EDC CX1\|SR[, multispeed: link at 10G\|1G][, soft rate select failed][, cage-presence check on\|, NVM word 0x2C unreadable (host interface), cage-presence check off]` | Start, X552 15ac module set up | always with `soft rate select failed` or `NVM word 0x2C unreadable`, else verbose |
| `stormnic-ixgbe: LOC 8086:DDDD: link setup: internal link iXFI forced\|KR (set at copper link-up), X557 advertising SPEEDS, AN restarted\|AN not restarted (veto)` | Start, X552 15ad | verbose |
| `stormnic-ixgbe: LOC 8086:DDDD: copper link up N Mb/s, internal link re-forced N time(s)` | Start, X552 15ad copper up (1000 or 10000) | verbose |
| `stormnic-ixgbe: LOC 8086:DDDD: copper link down` | Start, X552 15ad, no copper link within the wait | verbose |
| `stormnic-ixgbe: LOC 8086:DDDD: copper link up at a speed the internal link cannot carry (AN vendor status X)` | Start, X552 15ad, copper at 10/100 Mb/s | always |
| `stormnic-ixgbe: LOC 8086:DDDD: link up N Mb/s` | Start, link up (100, 1000, 2500 on X552, or 10000) | verbose (the summary line has it) |
| `stormnic-ixgbe: LOC 8086:DDDD: link up, speed encoding reserved` | Start, link up with a reserved speed field | verbose (the summary line has it) |
| `stormnic-ixgbe: LOC 8086:DDDD: link down after N ms (LINKS X, AUTOC X, AUTOC2 X, ESDP X)` | Start, no link (N is 9000 for 10GBASE-T, 3000 otherwise); the raw registers at the end of the wait, to tell a dead cable or partner (no signal detect, no PCS sync in LINKS) from a setup problem (#26) | verbose (the summary line has it) |
| `stormnic-ixgbe: LOC 8086:DDDD: DMA: 33 pages at device 0xX, RX 32 x 2048 B, TX 32 x 2048 B, legacy descriptors` | Start, DMA region mapped | verbose |
| `stormnic-ixgbe: LOC 8086:DDDD: DMA region not mapped: STATUS; releasing` | Start, AllocateBuffer or Map failed (returns DEVICE_ERROR) | always, after the kept steps |
| `stormnic-ixgbe: LOC 8086:DDDD: DMA check: broadcast frame sent, 60 bytes\|not sent within 100 ms (GPTC N)` | Start, link up | always when not sent, else verbose |
| `stormnic-ixgbe: LOC 8086:DDDD: DMA check: received N frame(s) after M ms (GPRC N), first L bytes from MAC to MAC type TTTT` | Start, link up, a frame arrived | verbose |
| `stormnic-ixgbe: LOC 8086:DDDD: DMA check: received nothing in 3000 ms (GPRC N)` | Start, link up, nothing arrived | verbose |
| `stormnic-ixgbe: LOC 8086:DDDD: rings started; DMA check skipped: link down` | Start, link down | verbose |
| `stormnic-ixgbe: LOC 8086:DDDD: rings failed: ERROR` | Start, a queue did not enable (e.g. `Timeout { register: 1028, .. }`) | always, after the kept steps |
| `stormnic-ixgbe: LOC 8086:DDDD: rings stopped[ (a frame was never sent)]` | Start, queues stopped after the check | always with `a frame was never sent`, else verbose |
| `stormnic-ixgbe: LOC 8086:DDDD: DMA region released; releasing` | Start, after `rings failed` (returns DEVICE_ERROR) | always |
| `stormnic-ixgbe: LOC: could not stop DMA: ERROR; bus mastering disabled\|could not be disabled, DMA region kept allocated` | Start or Stop, the queues would not stop | always, after the kept steps |
| `stormnic-ixgbe: could not unmap\|free the DMA region: STATUS` | Stop or failed Start | always |
| `stormnic-ixgbe 0.1.0: LOC 8086:DDDD NAME: MAC xx:xx:xx:xx:xx:xx, link up N Mb/s\|link up, speed encoding reserved\|link down after N ms (LINKS X, AUTOC X, AUTOC2 X, ESDP X), SNP installed` | Start, success: **the one line per NIC** (#22). On a link down the raw registers tell a dead cable or partner (no signal detect or PCS sync) from a setup problem (#26) | always |
| `stormnic-ixgbe: the N step(s) before the failure below[ (M earlier not kept)]:`, then those lines | before each "after the kept steps" failure when not verbose: the last 16 trace lines since this Start began | always |
| `stormnic-ixgbe: LOC 8086:DDDD: SNP not installed: STATUS; releasing` | Start, the child handle could not be made (returns DEVICE_ERROR) | always, after the kept steps |
| `stormnic-ixgbe: LOC: SNP initialized, MAC xx:xx:xx:xx:xx:xx, media present\|absent` | SNP Initialize (the first use: stormbootx's smoltcp) | verbose |
| `stormnic-ixgbe: LOC: SNP receive filters 0xNN, N multicast address(es)` | SNP ReceiveFilters changed the setting or set a list | verbose |
| `stormnic-ixgbe: LOC: SNP station address xx:xx:xx:xx:xx:xx` | SNP StationAddress | verbose |
| `stormnic-ixgbe: LOC: SNP shut down` | SNP Shutdown | verbose |
| `stormnic-ixgbe: LOC: SNP CALL failed: ERROR` | an SNP call failed in the NIC (DEVICE_ERROR) | always |
| `stormnic-ixgbe: LOC: Stop: SNP child removed` | Stop with the child | verbose |
| `stormnic-ixgbe: LOC: Stop: SNP child still in use (STATUS); kept` | Stop, the SNP is still open (returns DEVICE_ERROR) | always |
| `stormnic-ixgbe: LOC 8086:DDDD: Start could not open PciIo BY_DRIVER: STATUS` | Start, failure | always, after the kept steps |
| `stormnic-ixgbe: LOC 8086:DDDD: PCI attributes Get G, Supported S, Enable 0xE: SUCCESS; command 0xCCCC` | Start: memory decode and bus mastering through PciIo `Attributes` (G or S is a STATUS when that call failed) | verbose |
| `stormnic-ixgbe: LOC 8086:DDDD: PCI attributes … Enable: STATUS (accepted alone 0xE); command 0xC0 -> 0xC1 (set directly)` | Start: the firmware refused `Enable` (AMI Aptio 4, #19); MSE/BME set in the PCI command register instead | verbose |
| `stormnic-ixgbe: LOC 8086:DDDD: Start could not enable memory decode and bus mastering: command 0xCCCC after PCI attributes …` | Start, failure: MSE/BME still clear after the config write (returns UNSUPPORTED) | always, after the kept steps |
| `stormnic-ixgbe: LOC 8086:DDDD: Start could not enable memory decode and bus mastering: command register access failed: STATUS` | Start, failure (returns UNSUPPORTED) | always, after the kept steps |
| `stormnic-ixgbe: LOC 8086:DDDD NAME: bring-up failed: ERROR; releasing` | Start, quiesce/reset/NVM/MAC failed or the device went away (returns DEVICE_ERROR); ERROR is e.g. `Timeout { register: .., .. }`, `MissingNvm`, `InvalidMac`, `Removed`, `Io(..)`, or `Semaphore { .. }` when the reset's PHY semaphore is held | always, after the kept steps |
| `stormnic-ixgbe: could not undo the PCI decode and bus-master changes: STATUS` | Stop or failed Start | always |
| `stormnic-ixgbe: LOC: Stop: released` | Stop | verbose |
| `stormnic-ixgbe: Stop for a controller this driver never started` | Stop, unknown controller (returns DEVICE_ERROR) | always |

## Status

Verified on metal on **server3** (X9SRD-F, AMI Aptio 4 3.0a, 82599EN SFP+
8086:1557, MAC ac:1f:6b:8a:a4:5c) on 2026-10-01, booting the rustnic media
`golden-stormbootx-rustnic-416b7237c78a29a3` (this driver at 563ea8d): the
PCI decode fallback (#19), the reset with the CFG_DONE0 timeout reported
(#21), 10G SFI link setup on a passive DA cable and `link up 10000 Mb/s`
(#2, #13), the rings (#3) and the SNP (#4). stormbootx then printed `tcp4 :
smoltcp over SNP`, leased 192.168.16.104/20 over our SNP, reached the engine
and claimed boothost/server3. The SOL log is quoted in
[docs/snp.md](docs/snp.md#hardware-check-the-acceptance-for-4). The first
boot (#7) bound 8086:1557 on server1 with the scaffold.

Simulated tests (`sc-build scripts/test-hardware.sh`): 84 at 563ea8d, for
bring-up and PHY (including a bit-level I2C slave for the SFP+ EEPROM, port
expander and CS4227), rings, SNP and PCI decode.

Not verified on hardware:
- the X540 and X552 paths: no lab hardware (#15, #23). Start says so on the
  console when one runs (#17);
- 154f, QSFP+ (1558) and the X552 host-interface NVM read (#16): no lab
  hardware;
- Start's DMA check: on server3 it sent its frame but received nothing in
  3 s (`GPRC 0`), though the SNP received fine moments later (#24);
- the spec section 10 items the server3 log does not settle (cage-presence
  polarity with an empty cage, LNK_RST on a down link), listed in the
  [bring-up notes](docs/bring-up.md#hardware-checks-spec-section-10).

Open: the console trace is verbose on every boot (#22); the image is not
byte-reproducible across build directories (#12). `ipxe-intelx.efi` is retired:
stormbootx v0.17.0 ships no iPXE, and this driver is the X9 blades' Intel
driver on the rustnic media (#5).
