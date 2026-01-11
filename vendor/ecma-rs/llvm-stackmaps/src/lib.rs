//! LLVM StackMap v3 loader + parser + callsite lookup.
//!
//! This crate is intended for native runtimes that use LLVM's `gc.statepoint`
//! infrastructure. LLVM emits a `.llvm_stackmaps` section (StackMap format v3)
//! containing metadata for each safepoint. At runtime we:
//! - locate the `.llvm_stackmaps` bytes in the running process
//! - parse stackmap v3 records
//! - look up a record by return address (absolute PC)
//!
//! # Platform assumptions
//! - Linux (ELF)
//! - The final link step applies a linker script that defines
//!   `__stackmaps_start` / `__stackmaps_end` (see `runtime-native/stackmaps.ld`).
//! - `x86_64` or `aarch64`
//!
//! This is not a general purpose binary analysis crate; it implements the subset
//! needed to interpret stackmaps at runtime.

pub mod elf;
pub mod stackmap;

pub use stackmap::{
    stackmaps_bytes, Callsite, GcRootPair, LiveOut, Location, LocationKind, ParseError,
    StackMapFunction, StackMapHeader, StackMapRecord, StackMaps, StatepointRecordView,
};

#[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
mod global {
    use std::sync::OnceLock;

    use crate::stackmap::{stackmaps_bytes, ParseError, StackMaps};

    static STACKMAPS: OnceLock<Result<StackMaps, ParseError>> = OnceLock::new();

    pub fn stackmaps() -> Result<&'static StackMaps, &'static ParseError> {
        match STACKMAPS.get_or_init(|| StackMaps::parse(stackmaps_bytes())) {
            Ok(maps) => Ok(maps),
            Err(err) => Err(err),
        }
    }
}

/// Parse the in-process `.llvm_stackmaps` section and return a cached [`StackMaps`].
///
/// This is the typical entry point for a native runtime.
#[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
pub fn stackmaps() -> &'static StackMaps {
    match global::stackmaps() {
        Ok(maps) => maps,
        Err(err) => panic!("failed to parse .llvm_stackmaps: {err}"),
    }
}

/// Look up a stackmap record by callsite return address (absolute PC).
///
/// This uses a global cache built from the in-memory `.llvm_stackmaps` section.
#[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
pub fn lookup(pc: u64) -> Option<&'static StackMapRecord> {
    stackmaps().lookup(pc)
}

/// Look up and decode a `gc.statepoint` record by callsite return address.
///
/// Returns `None` when:
/// - no record exists for `pc`, or
/// - the record does not match the expected statepoint layout.
#[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
pub fn lookup_statepoint(pc: u64) -> Option<StatepointRecordView<'static>> {
    stackmaps().lookup_statepoint(pc)
}
