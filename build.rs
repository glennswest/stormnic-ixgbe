//! Link the image as an EFI boot-service driver, not an application.
//!
//! The PE32+ subsystem decides how the firmware loads the image (UEFI spec
//! 2.1.2): an application is unloaded when its entry point returns, a
//! boot-service driver stays resident in boot-services memory. The driver
//! binding installed by the entry point holds pointers into this image, so it
//! must stay. rustc's UEFI targets link `/SUBSYSTEM:EFI_APPLICATION` by
//! default; a later `/SUBSYSTEM` on the lld-link command line wins.
//!
//! `/Brepro` (#12): lld-link writes a hash of the image instead of the time
//! as the PE TimeDateStamp, and a content hash as the PDB GUID (random
//! otherwise). `/DEBUG:NONE`: no CodeView debug record, whose PDB path and
//! GUID still differed between build directories under /Brepro. The image is
//! stripped anyway and firmware never reads it. With the rustc wrapper in
//! `.cargo/config.toml`, two builds of the same source are byte-identical
//! wherever they run (`scripts/check-reproducible.sh`).

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let target = std::env::var("TARGET").unwrap_or_default();
    if target.ends_with("-uefi") {
        println!("cargo:rustc-link-arg-bins=/SUBSYSTEM:EFI_BOOT_SERVICE_DRIVER");
        println!("cargo:rustc-link-arg-bins=/Brepro");
        println!("cargo:rustc-link-arg-bins=/DEBUG:NONE");
    }
}
