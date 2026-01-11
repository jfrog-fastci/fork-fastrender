#[cfg(all(target_os = "linux", feature = "llvm_stackmaps_linker"))]
extern "C" {
  static __llvm_stackmaps_start: u8;
  static __llvm_stackmaps_end: u8;
}

/// Returns the bytes of the loaded `.llvm_stackmaps` section for the current
/// binary (Linux/ELF).
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

  #[cfg(not(all(target_os = "linux", feature = "llvm_stackmaps_linker")))]
  {
    &[]
  }
}

/// Stable API name: load stack maps for the current binary.
///
/// Currently only implemented for Linux/ELF via linker-defined symbols.
pub fn load_stackmaps_from_self() -> &'static [u8] {
  stackmaps_section()
}
