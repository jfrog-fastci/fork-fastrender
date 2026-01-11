//! Compatibility shim for the legacy `runtime_native::stackmap` API.
//!
//! The canonical LLVM StackMap v3 parser lives in [`crate::stackmaps`]. This
//! module exists for compatibility and provides:
//! - Re-exports of the canonical parser/types.
//! - A lazy global accessor ([`stackmaps`]) used by runtime stack walking / GC.
//!
//! ## Statepoint root enumeration
//! LLVM statepoints encode GC roots in the StackMap record's `locations` as:
//!
//! - 3 leading constant "header" locations (not roots):
//!   - callconv
//!   - flags
//!   - deopt operand count
//! - followed by `deopt_count` deopt operand locations (not roots)
//! - followed by `(base, derived)` pairs for each GC-live pointer at the safepoint.
//!
//! The runtime must treat **only** the post-header-and-deopt tail as base/derived pairs.
//! Note that the `"gc-live"(...)` operand bundle in IR is *not* necessarily the
//! full root set: LLVM's `rewrite-statepoints-for-gc` pass expands it based on
//! liveness.
//!
//! ## Record identity / lookup key
//! A StackMap record's `patchpoint_id` is **not unique** across callsites (it can
//! repeat). Runtime lookup/indexing must therefore be keyed by the callsite PC
//! (return address), i.e. `function_address + instruction_offset`.

use std::sync::OnceLock;

static STACKMAPS_INDEX: OnceLock<Option<crate::stackmaps::StackMaps>> = OnceLock::new();

/// Lazily parse and index the process' in-memory `.llvm_stackmaps` section.
///
/// This is intended for runtime stack walking / GC root enumeration. It panics
/// if stackmaps are unavailable or malformed.
pub fn stackmaps() -> &'static crate::stackmaps::StackMaps {
  try_stackmaps().unwrap_or_else(|| {
    panic!(
      "missing LLVM stackmaps in the current process: on Linux, enable feature `llvm_stackmaps_linker` (preferred) or ensure the main executable contains a stackmap section (e.g. `.data.rel.ro.llvm_stackmaps` / `.llvm_stackmaps`); on macOS, ensure LLVM emitted `__LLVM_STACKMAPS,__llvm_stackmaps`"
    )
  })
}

/// Lazily parse and index the process' in-memory `.llvm_stackmaps` section, returning `None` when
/// the section is missing.
pub fn try_stackmaps() -> Option<&'static crate::stackmaps::StackMaps> {
  STACKMAPS_INDEX
    .get_or_init(|| {
      let bytes = crate::stackmaps_section();
      if !bytes.is_empty() {
        return Some(crate::stackmaps::StackMaps::parse(bytes).unwrap_or_else(|err| {
          panic!("failed to parse .llvm_stackmaps section: {err}");
        }));
      }

      // Fallback for Linux x86_64 builds that don't use the linker-script based
      // stackmap boundary symbols. This loads stackmaps from the mapped main
      // executable (PIE/ASLR-safe).
      #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
      {
        crate::stackmaps::StackMaps::load_self().ok()
      }
      #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
      {
        None
      }
    })
    .as_ref()
}

pub use crate::stackmaps::{
  CallSite, CallsiteEntry, LiveOut, Location, StackMap, StackMapError, StackMapRecord, StackMaps,
  StackSizeRecord,
};
