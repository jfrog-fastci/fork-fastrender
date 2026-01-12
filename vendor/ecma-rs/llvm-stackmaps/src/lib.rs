//! LLVM StackMap v3 loader + parser + callsite lookup.
//!
//! This crate is intended for native runtimes that use LLVM's `gc.statepoint`
//! infrastructure. LLVM emits a `.llvm_stackmaps` section (StackMap format v3)
//! containing metadata for each safepoint. At runtime we:
//! - locate the `.llvm_stackmaps` bytes in the running process
//! - parse stackmap v3 records
//! - look up a record by return address (absolute PC)
//!
//! ## Return address lookup key
//! LLVM stackmap records are keyed by the *callsite return address*. For patchable statepoints
//! (non-zero `patch_bytes`), LLVM records the return address as the byte *after the reserved
//! patchable region*, not necessarily the byte after a literal `call` instruction.
//!
//! # Platform assumptions
//! - Linux (ELF) or macOS (Mach-O)
//! - On Linux, the final link step applies a linker script that defines
//!   `__start_llvm_stackmaps` / `__stop_llvm_stackmaps` (see:
//!   `runtime-native/link/stackmaps_nopie.ld` for non-PIE, or `runtime-native/link/stackmaps.ld` for
//!   PIE/DSO builds with stackmaps relocated into `.data.rel.ro.*`).
//!   That script also defines aliases: `__stackmaps_*`, `__fastr_stackmaps_*`, `__llvm_stackmaps_*`.
//! - On macOS, the stackmaps bytes are discovered via `getsectdatafromheader_64` in the main
//!   image, using the segment/section `__LLVM_STACKMAPS,__llvm_stackmaps`.
//! - `x86_64` or `aarch64`
//!
//! This is not a general purpose binary analysis crate; it implements the subset
//! needed to interpret stackmaps at runtime.

pub mod elf;
pub mod stackmap;
pub mod verify;

pub use stackmap::{
    stackmaps_bytes, Callsite, GcRootPair, LiveOut, Location, LocationKind, ParseError,
    StackMapFunction, StackMapHeader, StackMapRecord, StackMaps, StatepointRecordView,
};

#[cfg(any(
    all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
    all(target_os = "macos", any(target_arch = "x86_64", target_arch = "aarch64")),
))]
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
#[cfg(any(
    all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
    all(target_os = "macos", any(target_arch = "x86_64", target_arch = "aarch64")),
))]
pub fn stackmaps() -> &'static StackMaps {
    match global::stackmaps() {
        Ok(maps) => maps,
        Err(err) => panic!("failed to parse .llvm_stackmaps: {err}"),
    }
}

/// Look up a stackmap record by callsite return address (absolute PC).
///
/// This uses a global cache built from the in-memory `.llvm_stackmaps` section.
#[cfg(any(
    all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
    all(target_os = "macos", any(target_arch = "x86_64", target_arch = "aarch64")),
))]
pub fn lookup(pc: u64) -> Option<&'static StackMapRecord> {
    stackmaps().lookup(pc)
}

/// Look up a callsite index entry by return address (absolute PC).
///
/// This includes the record index plus per-function metadata like `stack_size`.
#[cfg(any(
    all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
    all(target_os = "macos", any(target_arch = "x86_64", target_arch = "aarch64")),
))]
pub fn lookup_callsite(pc: u64) -> Option<&'static Callsite> {
    stackmaps().lookup_callsite(pc)
}

/// Look up and decode a `gc.statepoint` record by callsite return address.
///
/// Returns `None` when:
/// - no record exists for `pc`, or
/// - the record does not match the expected statepoint layout.
#[cfg(any(
    all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
    all(target_os = "macos", any(target_arch = "x86_64", target_arch = "aarch64")),
))]
pub fn lookup_statepoint(pc: u64) -> Option<StatepointRecordView<'static>> {
    stackmaps().lookup_statepoint(pc)
}
