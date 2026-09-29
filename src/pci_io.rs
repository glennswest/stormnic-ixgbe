//! `EFI_PCI_IO_PROTOCOL`, from the UEFI specification (section 14.4).
//!
//! The `uefi` crate has the root-bridge protocol but not the per-device one,
//! which is what a PCI driver binds to. The layout here is the spec's, field
//! for field; only the calls the driver uses have safe wrappers.

use core::ffi::c_void;
use uefi::proto::unsafe_protocol;
use uefi::{Result, Status, StatusExt};

/// `EFI_PCI_IO_PROTOCOL_WIDTH`.
#[allow(dead_code)]
#[repr(u32)]
#[derive(Clone, Copy)]
pub enum Width {
    U8 = 0,
    U16 = 1,
    U32 = 2,
    U64 = 3,
}

/// `EFI_PCI_IO_PROTOCOL_ATTRIBUTE_OPERATION`.
#[allow(dead_code)]
#[repr(u32)]
#[derive(Clone, Copy)]
pub enum AttributeOp {
    Get = 0,
    Set = 1,
    Enable = 2,
    Disable = 3,
    Supported = 4,
}

/// `EFI_PCI_IO_ATTRIBUTE_MEMORY`: the function decodes its memory BARs.
pub const ATTRIBUTE_MEMORY: u64 = 0x0002;

/// `EFI_PCI_IO_PROTOCOL_OPERATION` (for `Map`).
#[allow(dead_code)]
#[repr(u32)]
#[derive(Clone, Copy)]
pub enum MapOp {
    BusMasterRead = 0,
    BusMasterWrite = 1,
    BusMasterCommonBuffer = 2,
}

type PollFn = unsafe extern "efiapi" fn(
    this: *mut PciIo,
    width: Width,
    bar_index: u8,
    offset: u64,
    mask: u64,
    value: u64,
    delay: u64,
    result: *mut u64,
) -> Status;

type IoMemFn = unsafe extern "efiapi" fn(
    this: *mut PciIo,
    width: Width,
    bar_index: u8,
    offset: u64,
    count: usize,
    buffer: *mut c_void,
) -> Status;

type ConfigFn = unsafe extern "efiapi" fn(
    this: *mut PciIo,
    width: Width,
    offset: u32,
    count: usize,
    buffer: *mut c_void,
) -> Status;

/// `EFI_PCI_IO_PROTOCOL_ACCESS`.
#[repr(C)]
pub struct Access {
    pub read: IoMemFn,
    pub write: IoMemFn,
}

/// `EFI_PCI_IO_PROTOCOL_CONFIG_ACCESS`.
#[repr(C)]
pub struct ConfigAccess {
    pub read: ConfigFn,
    pub write: ConfigFn,
}

/// `EFI_PCI_IO_PROTOCOL`.
#[repr(C)]
#[unsafe_protocol("4cf5b200-68b8-4ca5-9eec-b23e3f50029a")]
pub struct PciIo {
    pub poll_mem: PollFn,
    pub poll_io: PollFn,
    pub mem: Access,
    pub io: Access,
    pub pci: ConfigAccess,
    pub copy_mem: unsafe extern "efiapi" fn(
        this: *mut PciIo,
        width: Width,
        dest_bar_index: u8,
        dest_offset: u64,
        src_bar_index: u8,
        src_offset: u64,
        count: usize,
    ) -> Status,
    pub map: unsafe extern "efiapi" fn(
        this: *mut PciIo,
        operation: MapOp,
        host_address: *mut c_void,
        number_of_bytes: *mut usize,
        device_address: *mut u64,
        mapping: *mut *mut c_void,
    ) -> Status,
    pub unmap: unsafe extern "efiapi" fn(this: *mut PciIo, mapping: *mut c_void) -> Status,
    pub allocate_buffer: unsafe extern "efiapi" fn(
        this: *mut PciIo,
        alloc_type: u32,
        memory_type: u32,
        pages: usize,
        host_address: *mut *mut c_void,
        attributes: u64,
    ) -> Status,
    pub free_buffer:
        unsafe extern "efiapi" fn(this: *mut PciIo, pages: usize, host_address: *mut c_void) -> Status,
    pub flush: unsafe extern "efiapi" fn(this: *mut PciIo) -> Status,
    pub get_location: unsafe extern "efiapi" fn(
        this: *mut PciIo,
        segment: *mut usize,
        bus: *mut usize,
        device: *mut usize,
        function: *mut usize,
    ) -> Status,
    pub attributes: unsafe extern "efiapi" fn(
        this: *mut PciIo,
        operation: AttributeOp,
        attributes: u64,
        result: *mut u64,
    ) -> Status,
    pub get_bar_attributes: unsafe extern "efiapi" fn(
        this: *mut PciIo,
        bar_index: u8,
        supports: *mut u64,
        resources: *mut *mut c_void,
    ) -> Status,
    pub set_bar_attributes: unsafe extern "efiapi" fn(
        this: *mut PciIo,
        attributes: u64,
        bar_index: u8,
        offset: *mut u64,
        length: *mut u64,
    ) -> Status,
    pub rom_size: u64,
    pub rom_image: *mut c_void,
}

/// Where a function sits on the PCI bus, as `seg:bus:dev.fn`.
#[derive(Clone, Copy)]
pub struct Location {
    pub segment: usize,
    pub bus: usize,
    pub device: usize,
    pub function: usize,
}

impl core::fmt::Display for Location {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "{:04x}:{:02x}:{:02x}.{:x}",
            self.segment, self.bus, self.device, self.function
        )
    }
}

impl PciIo {
    fn this(&self) -> *mut PciIo {
        self as *const PciIo as *mut PciIo
    }

    /// One dword of the function's configuration space.
    pub fn config_read_u32(&self, offset: u32) -> Result<u32> {
        let mut v: u32 = 0;
        // SAFETY: `self` is a live PciIo interface opened by the caller, and
        // the buffer holds exactly one U32.
        unsafe {
            (self.pci.read)(self.this(), Width::U32, offset, 1, (&raw mut v).cast())
        }
        .to_result_with_val(|| v)
    }

    pub fn location(&self) -> Result<Location> {
        let mut l = Location { segment: 0, bus: 0, device: 0, function: 0 };
        // SAFETY: `self` is a live PciIo interface; the four out-pointers are
        // valid usizes.
        unsafe {
            (self.get_location)(
                self.this(),
                &raw mut l.segment,
                &raw mut l.bus,
                &raw mut l.device,
                &raw mut l.function,
            )
        }
        .to_result_with_val(|| l)
    }

    /// One dword at `offset` in memory BAR `bar`.
    pub fn mem_read_u32(&self, bar: u8, offset: u32) -> Result<u32> {
        let mut v: u32 = 0;
        // SAFETY: live PciIo interface; the buffer holds exactly one U32.
        unsafe {
            (self.mem.read)(self.this(), Width::U32, bar, offset.into(), 1, (&raw mut v).cast())
        }
        .to_result_with_val(|| v)
    }

    pub fn mem_write_u32(&self, bar: u8, offset: u32, value: u32) -> Result {
        let mut v = value;
        // SAFETY: live PciIo interface; the buffer holds exactly one U32,
        // which the firmware only reads.
        unsafe {
            (self.mem.write)(self.this(), Width::U32, bar, offset.into(), 1, (&raw mut v).cast())
        }
        .to_result()
    }

    /// `Attributes(op, attributes)`, returning the result word (meaningful
    /// for Get and Supported).
    pub fn attributes(&self, op: AttributeOp, attributes: u64) -> Result<u64> {
        let mut r: u64 = 0;
        // SAFETY: live PciIo interface; `r` is a valid out-pointer.
        unsafe { (self.attributes)(self.this(), op, attributes, &raw mut r) }.to_result_with_val(|| r)
    }
}
