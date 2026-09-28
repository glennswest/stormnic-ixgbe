# Changelog

## [Unreleased]

### 2026-09-27
- **feat:** Driver scaffold (#1): linked as an EFI boot-service driver (`build.rs`, `/SUBSYSTEM:EFI_BOOT_SERVICE_DRIVER`); the entry point installs `EFI_DRIVER_BINDING_PROTOCOL`. Supported matches Intel 82599/X540/X552 physical functions by PCI ID through `EFI_PCI_IO_PROTOCOL` (defined in `src/pci_io.rs` from the UEFI spec) and yields to a platform driver that already owns the NIC; Start holds PciIo BY_DRIVER, Stop releases it. Every bind, and every Intel network function not in the list, is logged to the console. `scripts/check-driver.sh` builds and checks the PE subsystem
- **chore:** Project created: a Rust `no_std` UEFI SNP driver for Intel 10G, replacing iPXE's `ipxe-intelx.efi` on the stormbootx media (stormbootx#26, #27). Scaffold only
