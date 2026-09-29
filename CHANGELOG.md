# Changelog

## [Unreleased]

### 2026-09-29
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
