//! Stackmap discovery via linker-exported `__fastr_stackmaps_start/end` symbols.
//!
//! `native-js`'s final link step injects a linker script fragment that:
//! - keeps stackmaps from `--gc-sections` (both LLVM's `.llvm_stackmaps` and the renamed
//!   `.data.rel.ro.llvm_stackmaps` used by PIE/DSO builds), and
//! - exports `__fastr_stackmaps_start` / `__fastr_stackmaps_end` as *globally linkable* symbols.
//!
//! This allows `runtime-native` to read the in-memory, relocated stackmap bytes without any ELF
//! parsing (`/proc/self/exe`, `dl_iterate_phdr`, etc.).

use core::{ptr, slice};

use crate::stackmaps::{StackMapError, StackMaps, STACKMAP_VERSION};

extern "C" {
  static __fastr_stackmaps_start: u8;
  static __fastr_stackmaps_end: u8;
}

/// Return the in-memory bytes of the executable's stackmaps section.
///
/// These bytes are taken directly from memory (not from `/proc/self/exe`) so they reflect any
/// relocations applied by the dynamic loader.
///
/// # Panics / aborts
/// This function aborts the process if the exported symbol range is invalid or empty. This is a
/// fatal configuration error for `native-js`-linked executables.
#[doc(hidden)]
pub fn stackmaps_bytes_from_exe() -> &'static [u8] {
  // Use `addr_of!` to avoid creating references to extern statics.
  let start = ptr::addr_of!(__fastr_stackmaps_start) as *const u8;
  let end = ptr::addr_of!(__fastr_stackmaps_end) as *const u8;

  let start_usize = start as usize;
  let end_usize = end as usize;
  if end_usize < start_usize {
    crate::trap::rt_trap_invalid_arg(
      "__fastr_stackmaps_end is before __fastr_stackmaps_start (invalid stackmaps range)",
    );
  }

  let len = end_usize - start_usize;
  if len == 0 {
    crate::trap::rt_trap_invalid_arg(
      "__fastr_stackmaps_start == __fastr_stackmaps_end (empty stackmaps section)",
    );
  }

  // Safety:
  // - `start..end` is expected to cover the stackmaps section, which is mapped for the
  //   lifetime of the process.
  // - `native-js` guarantees these symbols delimit the section (see native-js linker script
  //   fragment and tests).
  unsafe { slice::from_raw_parts(start, len) }
}

/// Parse the current executable's stackmaps section.
#[doc(hidden)]
pub fn stackmaps_from_exe() -> Result<StackMaps, StackMapError> {
  let bytes = stackmaps_bytes_from_exe();

  // Sanity check before parsing so failures are easier to interpret.
  //
  // Note: The linker may insert alignment padding *inside* the output section before the first
  // input section payload (e.g. if the first stackmaps input section has 16-byte
  // alignment but the output section is only 8-byte aligned). That padding is typically
  // zero-filled, so we skip leading zeros here rather than assuming the v3 header starts at
  // `bytes[0]`.
  let version = bytes.iter().copied().find(|&b| b != 0).unwrap_or(0);
  if version != STACKMAP_VERSION {
    return Err(StackMapError::UnsupportedVersion(version));
  }

  StackMaps::parse(bytes)
}
