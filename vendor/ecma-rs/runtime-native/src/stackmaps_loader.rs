#[cfg(all(target_os = "linux", feature = "llvm_stackmaps_linker"))]
extern "C" {
  // Exported by the native-js link pipeline (and by `runtime-native/stackmaps.ld`
  // when linking via Cargo with the `llvm_stackmaps_linker` feature).
  static __fastr_stackmaps_start: u8;
  static __fastr_stackmaps_end: u8;
}

#[cfg(target_os = "macos")]
mod macho {
  use core::slice;

  // We only need an opaque handle for the Mach-O header pointer.
  #[repr(C)]
  pub struct MachHeader64 {
    _opaque: [u8; 0],
  }

  extern "C" {
    pub fn _dyld_get_image_header(image_index: u32) -> *const core::ffi::c_void;
    pub fn getsectdatafromheader_64(
      mh: *const MachHeader64,
      segname: *const libc::c_char,
      sectname: *const libc::c_char,
      size: *mut u64,
    ) -> *const u8;
  }

  /// Best-effort lookup of LLVM stackmaps in the main Mach-O image.
  ///
  /// LLVM typically emits stackmaps into segment/section:
  /// `__LLVM_STACKMAPS,__llvm_stackmaps`.
  pub unsafe fn stackmaps_section() -> &'static [u8] {
    let header = _dyld_get_image_header(0);
    if header.is_null() {
      return &[];
    }

    let mut size: u64 = 0;
    let ptr = getsectdatafromheader_64(
      header.cast::<MachHeader64>(),
      b"__LLVM_STACKMAPS\0".as_ptr().cast(),
      b"__llvm_stackmaps\0".as_ptr().cast(),
      &mut size,
    );

    if ptr.is_null() || size == 0 {
      return &[];
    }

    let len = match usize::try_from(size) {
      Ok(len) => len,
      Err(_) => {
        panic!(".llvm_stackmaps size overflow on macOS: size={size}");
      }
    };

    // StackMap v3 payload contains 64-bit fields and is 8-byte aligned.
    if (ptr as usize) % 8 != 0 {
      panic!(
        ".llvm_stackmaps pointer misaligned on macOS: ptr={:#x}",
        ptr as usize
      );
    }
    if len % 8 != 0 {
      panic!(".llvm_stackmaps length misaligned on macOS: len={len}");
    }

    slice::from_raw_parts(ptr, len)
  }
}

/// Returns the bytes of the loaded `.llvm_stackmaps` section for the current binary.
///
/// The boundaries are provided by linker-defined symbols from `stackmaps.ld`:
/// `__fastr_stackmaps_start` and `__fastr_stackmaps_end`.
pub fn stackmaps_section() -> &'static [u8] {
  #[cfg(all(target_os = "linux", feature = "llvm_stackmaps_linker"))]
  unsafe {
    let start = core::ptr::addr_of!(__fastr_stackmaps_start);
    let end = core::ptr::addr_of!(__fastr_stackmaps_end);

    let start_addr = start as usize;
    let end_addr = end as usize;
    if end_addr < start_addr {
      panic!(
        "invalid .llvm_stackmaps range: __fastr_stackmaps_end ({end_addr:#x}) < __fastr_stackmaps_start ({start_addr:#x})"
      );
    }

    let len = end_addr - start_addr;

    // Stack maps are metadata; if this is enormous something went very wrong
    // (e.g. the linker script wasn't applied and symbols resolved to unrelated
    // addresses).
    const MAX_LEN: usize = 512 * 1024 * 1024; // 512 MiB
    if len > MAX_LEN {
      panic!(
        "invalid .llvm_stackmaps length: {len} bytes (max {MAX_LEN}); linker script probably not applied"
      );
    }

    // StackMap v3 payload contains 64-bit fields and is 8-byte aligned.
    if start_addr % 8 != 0 {
      panic!(
        ".llvm_stackmaps pointer misaligned on Linux: start={start_addr:#x}"
      );
    }
    if len % 8 != 0 {
      panic!(".llvm_stackmaps length misaligned on Linux: len={len}");
    }

    core::slice::from_raw_parts(start, len)
  }

  #[cfg(target_os = "macos")]
  unsafe {
    return macho::stackmaps_section();
  }

  #[cfg(not(any(all(target_os = "linux", feature = "llvm_stackmaps_linker"), target_os = "macos")))]
  &[]
}

/// Stable API name: load stack maps for the current binary.
///
/// Currently implemented for:
/// - Linux/ELF via linker-defined symbols from `stackmaps.ld` (feature: `llvm_stackmaps_linker`)
/// - macOS/Mach-O via `getsectdatafromheader_64` (`__LLVM_STACKMAPS,__llvm_stackmaps`)
pub fn load_stackmaps_from_self() -> &'static [u8] {
  stackmaps_section()
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
  use super::stackmaps_section;
  use crate::StackMaps;

  #[repr(align(8))]
  struct Aligned<const N: usize>([u8; N]);

  // Minimal StackMap v3 header with zero functions/constants/records.
  #[used]
  #[link_section = "__LLVM_STACKMAPS,__llvm_stackmaps"]
  static TEST_STACKMAP_BLOB: Aligned<16> = Aligned([
    3, 0, 0, 0, // version + reserved
    0, 0, 0, 0, // num_functions
    0, 0, 0, 0, // num_constants
    0, 0, 0, 0, // num_records
  ]);

  #[test]
  fn discovers_macho_stackmaps_section_and_parses() {
    let section = stackmaps_section();
    assert!(
      section.len() >= TEST_STACKMAP_BLOB.0.len(),
      "expected .llvm_stackmaps section to be present"
    );
    assert_eq!(&section[..16], &TEST_STACKMAP_BLOB.0);

    let parsed = StackMaps::parse(section).expect("stackmaps should parse");
    assert_eq!(parsed.raw().version, 3);
    assert_eq!(parsed.raw().records.len(), 0);
  }
}
