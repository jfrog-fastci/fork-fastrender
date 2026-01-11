use core::{ptr, slice};

/// Return the in-memory `.llvm_stackmaps` section as a byte slice.
///
/// This relies on GNU ld / lld's start/stop symbol convention:
/// - `__start_.llvm_stackmaps`
/// - `__stop_.llvm_stackmaps`
///
/// Note: the `mold` linker does **not** currently synthesize these symbols. If
/// the final binary is linked with mold and `stackmaps_bytes()` is referenced,
/// linking will fail with undefined symbol errors.
///
/// The section name includes a dot, so we must use `#[link_name]` to reference
/// the raw ELF symbol names.
///
/// # Panics / Safety
/// This function assumes the section is present and mapped into memory. If the
/// final binary was linked without `.llvm_stackmaps`, linking will fail due to
/// missing `__start_/__stop_` symbols.
#[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
pub fn stackmaps_bytes() -> &'static [u8] {
    // SAFETY:
    // - The linker defines `__start_*/__stop_*` as byte pointers.
    // - The section is mapped read-only into the process.
    unsafe {
        extern "C" {
            #[link_name = "__start_.llvm_stackmaps"]
            static START: u8;
            #[link_name = "__stop_.llvm_stackmaps"]
            static STOP: u8;
        }

        let start = ptr::addr_of!(START);
        let stop = ptr::addr_of!(STOP);
        let len = (stop as usize).saturating_sub(start as usize);
        slice::from_raw_parts(start, len)
    }
}

#[cfg(not(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64"))))]
pub fn stackmaps_bytes() -> &'static [u8] {
    &[]
}
