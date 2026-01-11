#[cfg(all(target_os = "linux", feature = "llvm_stackmaps_linker"))]
extern "C" {
  static __llvm_stackmaps_start: u8;
  static __llvm_stackmaps_end: u8;
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
/// `__llvm_stackmaps_start` and `__llvm_stackmaps_end`.
pub fn stackmaps_section() -> &'static [u8] {
  #[cfg(all(target_os = "linux", feature = "llvm_stackmaps_linker"))]
  unsafe {
    let start = core::ptr::addr_of!(__llvm_stackmaps_start) as usize;
    let end = core::ptr::addr_of!(__llvm_stackmaps_end) as usize;

    if end < start {
      panic!(
        "invalid .llvm_stackmaps range: __llvm_stackmaps_end ({end:#x}) < __llvm_stackmaps_start ({start:#x})"
      );
    }

    let len = end - start;

    // Stack maps are metadata; if this is enormous something went very wrong
    // (e.g. the linker script wasn't applied and symbols resolved to unrelated
    // addresses).
    const MAX_LEN: usize = 512 * 1024 * 1024; // 512 MiB
    if len > MAX_LEN {
      panic!(
        "invalid .llvm_stackmaps length: {len} bytes (max {MAX_LEN}); linker script probably not applied"
      );
    }

    core::slice::from_raw_parts(start as *const u8, len)
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
