// `cargo-fuzz` builds dependencies with `--cfg fuzzing`. Rust's `unexpected_cfgs` lint warns about
// unknown cfg names in normal builds, so allow it in this module.
#![allow(unexpected_cfgs)]

use core::fmt;

#[cfg(any(
    all(not(fuzzing), target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
    all(not(fuzzing), target_os = "macos", any(target_arch = "x86_64", target_arch = "aarch64")),
))]
use core::{ptr, slice};

#[derive(Debug, Clone, Copy)]
pub enum LoadError {
    InvalidRange { start: usize, end: usize },
    TooLarge { len: usize, max: usize },
    Misaligned { addr: usize },
    SizeOverflow { size: u64 },
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadError::InvalidRange { start, end } => write!(
                f,
                "invalid .llvm_stackmaps range: end ({end:#x}) < start ({start:#x})"
            ),
            LoadError::TooLarge { len, max } => {
                write!(f, "invalid .llvm_stackmaps length: {len} bytes (max {max})")
            }
            LoadError::Misaligned { addr } => {
                write!(f, ".llvm_stackmaps pointer misaligned: addr={addr:#x}")
            }
            LoadError::SizeOverflow { size } => {
                write!(f, ".llvm_stackmaps size overflow: size={size}")
            }
        }
    }
}

impl std::error::Error for LoadError {}

/// Return the in-memory `.llvm_stackmaps` section as a byte slice.
///
/// Platform behavior:
/// - **Linux/ELF:** uses linker-script-defined range symbols:
///   - `__start_llvm_stackmaps`
///   - `__stop_llvm_stackmaps`
///   The repo provides ready-to-use linker script fragments:
///   - **non-PIE executables:** `vendor/ecma-rs/runtime-native/link/stackmaps_nopie.ld`
///     (keeps `.llvm_stackmaps`)
///   - **PIE/DSO builds (lld):** `vendor/ecma-rs/runtime-native/link/stackmaps.ld`
///     (expects stackmaps in `.data.rel.ro.llvm_stackmaps`, e.g. after an `llvm-objcopy
///     --rename-section` step)
///   - **PIE/DSO builds (GNU ld):** `vendor/ecma-rs/runtime-native/link/stackmaps_gnuld.ld`
///   The legacy path `vendor/ecma-rs/runtime-native/stackmaps.ld` is kept as a
///   compatibility shim.
///
///   These fragments also define:
///   - `__stackmaps_start` / `__stackmaps_end` (generic aliases)
///   - `__fastr_stackmaps_*` (project-specific aliases)
///   - `__llvm_stackmaps_*` (legacy aliases)
/// - **macOS/Mach-O:** uses `getsectdatafromheader_64` to locate the stackmaps section in the
///   main image (`__LLVM_STACKMAPS,__llvm_stackmaps`).
///
/// # Errors / Safety
/// This function assumes the section is present and mapped into memory. If the
/// final binary was linked without applying a linker script that defines these
/// symbols, linking will fail due to missing `__start_llvm_stackmaps`/`__stop_llvm_stackmaps`.
///
/// If the linker symbols resolve but produce an invalid range (misaligned, inverted, or
/// implausibly large), this function returns an error instead of panicking.
//
// When fuzzing, we do not link an executable with a real `.llvm_stackmaps` section or linker script
// range symbols; provide a stub to keep the parser fuzz targets buildable.
#[cfg(all(
    fuzzing,
    any(
        all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
        all(target_os = "macos", any(target_arch = "x86_64", target_arch = "aarch64")),
    )
))]
pub fn stackmaps_bytes() -> Result<&'static [u8], LoadError> {
    Ok(&[])
}

#[cfg(all(
    not(fuzzing),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub fn stackmaps_bytes() -> Result<&'static [u8], LoadError> {
    // SAFETY:
    // - The linker script defines `__start_llvm_stackmaps`/`__stop_llvm_stackmaps` as byte pointers.
    // - The section is mapped read-only into the process.
    unsafe {
        extern "C" {
            static __start_llvm_stackmaps: u8;
            static __stop_llvm_stackmaps: u8;
        }

        let start = ptr::addr_of!(__start_llvm_stackmaps);
        let end = ptr::addr_of!(__stop_llvm_stackmaps);
        let start_addr = start as usize;
        let end_addr = end as usize;
        let len = end_addr
            .checked_sub(start_addr)
            .ok_or(LoadError::InvalidRange {
                start: start_addr,
                end: end_addr,
            })?;

        // Stack maps are metadata; if this is enormous something went very wrong (e.g. the linker
        // script wasn't applied and the symbols resolved to unrelated addresses).
        const MAX_LEN: usize = 512 * 1024 * 1024; // 512 MiB
        if len > MAX_LEN {
            return Err(LoadError::TooLarge { len, max: MAX_LEN });
        }

        // StackMap v3 records are 8-byte aligned, so the section start should be aligned.
        //
        // However, some toolchains/linkers have been observed to leave short trailing padding/noise
        // bytes at the end of the output section, so don't require `len % 8 == 0`.
        if start_addr % 8 != 0 {
            return Err(LoadError::Misaligned { addr: start_addr });
        }

        Ok(slice::from_raw_parts(start, len))
    }
}

#[cfg(all(
    not(fuzzing),
    target_os = "macos",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub fn stackmaps_bytes() -> Result<&'static [u8], LoadError> {
    unsafe {
        #[repr(C)]
        struct MachHeader64 {
            _opaque: [u8; 0],
        }

        extern "C" {
            fn _dyld_get_image_header(image_index: u32) -> *const core::ffi::c_void;
            fn getsectdatafromheader_64(
                mh: *const MachHeader64,
                segname: *const core::ffi::c_char,
                sectname: *const core::ffi::c_char,
                size: *mut u64,
            ) -> *const u8;
        }

        // Main image is index 0.
        let header = _dyld_get_image_header(0);
        if header.is_null() {
            return Ok(&[]);
        }

        let mut size: u64 = 0;
        let ptr = getsectdatafromheader_64(
            header.cast::<MachHeader64>(),
            b"__LLVM_STACKMAPS\0".as_ptr().cast(),
            b"__llvm_stackmaps\0".as_ptr().cast(),
            &mut size,
        );
        if ptr.is_null() || size == 0 {
            return Ok(&[]);
        }

        let len = usize::try_from(size).map_err(|_| LoadError::SizeOverflow { size })?;

        // Stack maps are metadata; if this is enormous something went very wrong.
        const MAX_LEN: usize = 512 * 1024 * 1024; // 512 MiB
        if len > MAX_LEN {
            return Err(LoadError::TooLarge { len, max: MAX_LEN });
        }

        // StackMap v3 payload contains 64-bit fields and records are 8-byte aligned.
        let addr = ptr as usize;
        if addr % 8 != 0 {
            return Err(LoadError::Misaligned { addr });
        }

        Ok(slice::from_raw_parts(ptr, len))
    }
}

#[cfg(not(any(
    all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
    all(target_os = "macos", any(target_arch = "x86_64", target_arch = "aarch64")),
)))]
pub fn stackmaps_bytes() -> Result<&'static [u8], LoadError> {
    Ok(&[])
}

#[cfg(all(test, target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
mod tests {
    use super::stackmaps_bytes;

    // Define a tiny `.llvm_stackmaps` section plus `__{start,stop}_llvm_stackmaps`
    // so we can exercise the loader without needing to involve a custom linker
    // script in unit tests.
    core::arch::global_asm!(
        r#"
        .section .llvm_stackmaps,"a",@progbits
        .p2align 3
        .globl __start_llvm_stackmaps
        __start_llvm_stackmaps:
        .byte 0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0xBA, 0xBE, 0xAA
        .globl __stop_llvm_stackmaps
        __stop_llvm_stackmaps:
        .text
        "#
    );

    #[test]
    fn stackmaps_bytes_reads_section_range() {
        assert_eq!(
            stackmaps_bytes().unwrap(),
            &[0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0xBA, 0xBE, 0xAA]
        );
    }
}

#[cfg(all(test, target_os = "macos", any(target_arch = "x86_64", target_arch = "aarch64")))]
mod macos_tests {
    use super::stackmaps_bytes;

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
    fn stackmaps_bytes_discovers_macho_section() {
        let bytes = stackmaps_bytes().unwrap();
        assert!(
            bytes.len() >= TEST_STACKMAP_BLOB.0.len(),
            "expected stackmaps_bytes() to locate __LLVM_STACKMAPS,__llvm_stackmaps section"
        );
        assert_eq!(&bytes[..16], &TEST_STACKMAP_BLOB.0);
    }
}
