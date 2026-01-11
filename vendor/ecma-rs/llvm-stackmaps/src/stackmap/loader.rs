use core::{ptr, slice};

/// Return the in-memory `.llvm_stackmaps` section as a byte slice.
///
/// Platform behavior:
/// - **Linux/ELF:** uses linker-script-defined range symbols:
///   - `__start_llvm_stackmaps`
///   - `__stop_llvm_stackmaps`
///   The repo provides a ready-to-use script fragment at
///   `vendor/ecma-rs/runtime-native/link/stackmaps.ld` (lld-oriented) or
///   `vendor/ecma-rs/runtime-native/link/stackmaps_gnuld.ld` (GNU ld-oriented). The
///   legacy path `vendor/ecma-rs/runtime-native/stackmaps.ld` is kept as a
///   compatibility shim. The fragment also defines:
///   - `__stackmaps_start` / `__stackmaps_end` (generic aliases)
///   - `__fastr_stackmaps_*` (project-specific aliases)
///   - `__llvm_stackmaps_*` (legacy aliases)
/// - **macOS/Mach-O:** uses `getsectdatafromheader_64` to locate the stackmaps section in the
///   main image (`__LLVM_STACKMAPS,__llvm_stackmaps`).
///
/// # Panics / Safety
/// This function assumes the section is present and mapped into memory. If the
/// final binary was linked without applying a linker script that defines these
/// symbols, linking will fail due to missing `__start_llvm_stackmaps`/`__stop_llvm_stackmaps`.
#[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
pub fn stackmaps_bytes() -> &'static [u8] {
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
        assert!(
            end_addr >= start_addr,
            "invalid .llvm_stackmaps range: __stop_llvm_stackmaps ({end_addr:#x}) < __start_llvm_stackmaps ({start_addr:#x})"
        );

        let len = end_addr - start_addr;

        // Stack maps are metadata; if this is enormous something went very wrong (e.g. the linker
        // script wasn't applied and the symbols resolved to unrelated addresses).
        const MAX_LEN: usize = 512 * 1024 * 1024; // 512 MiB
        assert!(
            len <= MAX_LEN,
            "invalid .llvm_stackmaps length: {len} bytes (max {MAX_LEN}); linker script probably not applied"
        );

        // StackMap v3 payload contains 64-bit fields and records are 8-byte aligned.
        assert!(
            start_addr % 8 == 0,
            ".llvm_stackmaps pointer misaligned: start={start_addr:#x}"
        );
        assert!(
            len % 8 == 0,
            ".llvm_stackmaps length misaligned: len={len} (expected multiple of 8)"
        );

        slice::from_raw_parts(start, len)
    }
}

#[cfg(all(target_os = "macos", any(target_arch = "x86_64", target_arch = "aarch64")))]
pub fn stackmaps_bytes() -> &'static [u8] {
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
            Err(_) => panic!(".llvm_stackmaps size overflow on macOS: size={size}"),
        };

        // Stack maps are metadata; if this is enormous something went very wrong.
        const MAX_LEN: usize = 512 * 1024 * 1024; // 512 MiB
        assert!(
            len <= MAX_LEN,
            "invalid .llvm_stackmaps length: {len} bytes (max {MAX_LEN})"
        );

        // StackMap v3 payload contains 64-bit fields and records are 8-byte aligned.
        assert!(
            (ptr as usize) % 8 == 0,
            ".llvm_stackmaps pointer misaligned: ptr={:#x}",
            ptr as usize
        );
        assert!(
            len % 8 == 0,
            ".llvm_stackmaps length misaligned: len={len} (expected multiple of 8)"
        );

        slice::from_raw_parts(ptr, len)
    }
}

#[cfg(not(any(
    all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
    all(target_os = "macos", any(target_arch = "x86_64", target_arch = "aarch64")),
)))]
pub fn stackmaps_bytes() -> &'static [u8] {
    &[]
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
        .byte 0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0xBA, 0xBE
        .globl __stop_llvm_stackmaps
        __stop_llvm_stackmaps:
        .text
        "#
    );

    #[test]
    fn stackmaps_bytes_reads_section_range() {
        assert_eq!(
            stackmaps_bytes(),
            &[0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0xBA, 0xBE]
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
        let bytes = stackmaps_bytes();
        assert!(
            bytes.len() >= TEST_STACKMAP_BLOB.0.len(),
            "expected stackmaps_bytes() to locate __LLVM_STACKMAPS,__llvm_stackmaps section"
        );
        assert_eq!(&bytes[..16], &TEST_STACKMAP_BLOB.0);
    }
}
