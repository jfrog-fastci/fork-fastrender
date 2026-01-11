use core::{ptr, slice};

/// Return the in-memory `.llvm_stackmaps` section as a byte slice.
///
/// This relies on linker-script-defined range symbols:
/// - `__stackmaps_start`
/// - `__stackmaps_end`
///
/// The repo provides a ready-to-use script fragment at
/// `vendor/ecma-rs/runtime-native/stackmaps.ld`, which also defines project-
/// specific aliases (`__fastr_stackmaps_*`) and legacy aliases
/// (`__llvm_stackmaps_*`).
///
/// # Panics / Safety
/// This function assumes the section is present and mapped into memory. If the
/// final binary was linked without applying a linker script that defines these
/// symbols, linking will fail due to missing `__stackmaps_*` symbols.
#[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
pub fn stackmaps_bytes() -> &'static [u8] {
    // SAFETY:
    // - The linker script defines `__stackmaps_*` as byte pointers.
    // - The section is mapped read-only into the process.
    unsafe {
        extern "C" {
            static __stackmaps_start: u8;
            static __stackmaps_end: u8;
        }

        let start = ptr::addr_of!(__stackmaps_start);
        let end = ptr::addr_of!(__stackmaps_end);
        let start_addr = start as usize;
        let end_addr = end as usize;
        assert!(
            end_addr >= start_addr,
            "invalid .llvm_stackmaps range: __stackmaps_end ({end_addr:#x}) < __stackmaps_start ({start_addr:#x})"
        );
        slice::from_raw_parts(start, end_addr - start_addr)
    }
}

#[cfg(not(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64"))))]
pub fn stackmaps_bytes() -> &'static [u8] {
    &[]
}

#[cfg(all(test, target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
mod tests {
    use super::stackmaps_bytes;

    // Define a tiny `.llvm_stackmaps` section plus `__stackmaps_{start,end}`
    // so we can exercise the loader without needing to involve a custom linker
    // script in unit tests.
    core::arch::global_asm!(
        r#"
        .section .llvm_stackmaps,"a",@progbits
        .globl __stackmaps_start
        __stackmaps_start:
        .byte 0xDE, 0xAD, 0xBE, 0xEF
        .globl __stackmaps_end
        __stackmaps_end:
        .text
        "#
    );

    #[test]
    fn stackmaps_bytes_reads_section_range() {
        assert_eq!(stackmaps_bytes(), &[0xDE, 0xAD, 0xBE, 0xEF]);
    }
}
