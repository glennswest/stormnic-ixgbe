# Changelog

## [Unreleased]

### 2026-09-29
- **docs:** `docs/spec/phy.md`, an independent PHY and link programming specification for 82599 / X540 / X552 (MDIO, SW/FW semaphores, SFP+ I2C and SFF-8472, 82599 AUTOC/SFI/KR/KX4, X540 10GBASE-T, X552 IOSF KR registers, CS4227, X557 and Marvell paths, LINKS, timeouts, per-device polling walkthroughs), written from Intel's BSD-licensed shared code; `NOTICE` carries Intel's BSD-3-Clause notice (#13).
- **feat:** X552 link setup per device (#2), from Intel's BSD-3-Clause shared code (owner's answer on #2): 15ab KR PHY advertises KR+KX over the IOSF sideband and restarts AN (skipped on manageability veto); 15ac resets the shared CS4227 once per power-on over bit-banged I2C, identifies the SFP+ module, sets the KR PHY to its speed and the CS4227 EDC mode; 15ad finds the X557 on MDIO, releases its power-up stall, forces the internal iXFI link (or KR) and re-forces it to 1G when copper links at 1G; 15aa and 15ae are run by hardware/firmware. SW_FW_SYNC is never taken from another owner. New console lines; 13 new simulated tests including a bit-level I2C slave.
- **docs:** README (X552 behaviour, console lines, errors), bring-up notes (source and BSD notice, registers, per-device table, limits, verification at 8407e3c), work plan.
- **build:** Commit `Cargo.lock` (generated on dev with sc-build: uefi 0.39.0, uefi-raw 0.15.1) and build `--locked` in `scripts/check-driver.sh`, so stormbootx's pinned-commit build of the driver is reproducible (#11). README says how to build a pinned commit locked.
- **docs:** README (Start behaviour, every new console line, server1 is 8086:1557), bring-up notes (82599/X540 datasheet basis, known limits, remote verification at 025a137) and work plan; X552 PHY setup remains blocked on vendor documentation.
- **feat:** Start brings the NIC up (#2): enables PCI memory decode (original attributes restored on Stop or failure), runs the common reset and NVM MAC read over BAR0, then link setup per family — 82599 applies its NVM-loaded AUTOC/AUTOC2 link mode with Restart_AN; X540's integrated PHY negotiates from its NVM image with no writes; X552 PHY setup is not implemented (vendor documentation pending) and is logged as such — and waits up to 3 s for link. Each step, the 82599 link mode/ESDP, and the link result are logged; a failed bring-up releases the NIC with DEVICE_ERROR. Five new simulated-register tests.
- **docs:** Record passing remote regression tests and UEFI image validation at 50b6bb5, with the existing local build-log append limitation; PHY bring-up remains unimplemented.
- **docs:** Record specific X552 internal-register and CS4227 programming-document gaps, vendor references, and continuation requirements for #2; retain the approved all-variant scope.
- **docs:** Resume #2 with the owner-approved all-22-ID scope and record the vendor-documentation audit, integration, and remote verification plan.

### 2026-09-28
- **docs:** Record successful remote verification of the #2 common primitives (10 tests and UEFI PE check at fed1c3b); leave PHY setup and binding integration pending the owner support-scope decision.
- **refactor:** Add datasheet-based, UEFI-independent reset, NVM-provisioned MAC, and current link-status primitives with bounded failure paths and simulated-register tests for #2; binding integration awaits PHY scope resolution.
- **docs:** Record the datasheet bring-up implementation and verification plan for #2.
- **docs:** Refreshed README and CLAUDE.md from the code: the full 22-ID table and the class-code and VF rules, `scripts/check-driver.sh`, no configuration or ports, every console line exactly as printed, how it ships (stormbootx#29, no golden), and the platform-driver rule described as the BY_DRIVER PciIo check it is
- **docs:** Work plan: #1 builds and checks as a boot-service driver on dev (`sc-build scripts/check-driver.sh` at ed58e7c); the server1 boot waits on stormbootx#29, which gets the `.efi` onto the stormbootx media

### 2026-09-27
- **docs:** README describes the driver binding, its console lines, and not to ship it beside `ipxe-intelx.efi` until the SNP lands
- **feat:** Driver scaffold (#1): linked as an EFI boot-service driver (`build.rs`, `/SUBSYSTEM:EFI_BOOT_SERVICE_DRIVER`); the entry point installs `EFI_DRIVER_BINDING_PROTOCOL`. Supported matches Intel 82599/X540/X552 physical functions by PCI ID through `EFI_PCI_IO_PROTOCOL` (defined in `src/pci_io.rs` from the UEFI spec) and yields to a platform driver that already owns the NIC; Start holds PciIo BY_DRIVER, Stop releases it. Every bind, and every Intel network function not in the list, is logged to the console. `scripts/check-driver.sh` builds and checks the PE subsystem
- **chore:** Project created: a Rust `no_std` UEFI SNP driver for Intel 10G, replacing iPXE's `ipxe-intelx.efi` on the stormbootx media (stormbootx#26, #27). Scaffold only
