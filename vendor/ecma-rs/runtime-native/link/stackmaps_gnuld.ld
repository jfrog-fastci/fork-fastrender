/* GNU ld-only linker script fragment (Linux/ELF) for retaining and exposing LLVM stack maps.
 *
 * This is the GNU ld counterpart to `runtime-native/link/stackmaps.ld`.
 *
 * Why a separate fragment?
 * - Prefer `runtime-native/link/stackmaps.ld`: it anchors at `INSERT AFTER .text;`
 *   for lld. lld keeps the writable stackmaps output section in the data PT_LOAD
 *   (avoiding RWX) and preserves stackmaps under `--gc-sections`.
 * - This file is a GNU ld-specific alternative that anchors at `INSERT BEFORE .dynamic;`
 *   to force the stackmaps output section into the RELRO/data region in
 *   dynamically-linked binaries. This avoids GNU ld merging the injected
 *   writable section into the text PT_LOAD in PIE mode (which would become RWX
 *   during relocation).
 *
 * Note: `.dynamic` must exist for this fragment to be usable. This is true for
 * the dynamically-linked PIE/non-PIE binaries produced by our toolchain.
 */

SECTIONS {
  .data.rel.ro.llvm_stackmaps : ALIGN(8) {
    __start_llvm_stackmaps = .;
    __fastr_stackmaps_start = .;
    __stackmaps_start = .;
    __llvm_stackmaps_start = .;
    KEEP(*(.data.rel.ro.llvm_stackmaps))
    KEEP(*(.data.rel.ro.llvm_stackmaps.*))
    KEEP(*(.llvm_stackmaps))
    KEEP(*(.llvm_stackmaps.*))
    __stop_llvm_stackmaps = .;
    __fastr_stackmaps_end = .;
    __stackmaps_end = .;
    __llvm_stackmaps_end = .;
  }

  .data.rel.ro.llvm_faultmaps : ALIGN(8) {
    __llvm_faultmaps_start = .;
    KEEP(*(.data.rel.ro.llvm_faultmaps))
    KEEP(*(.data.rel.ro.llvm_faultmaps.*))
    KEEP(*(.llvm_faultmaps))
    KEEP(*(.llvm_faultmaps.*))
    __llvm_faultmaps_end = .;
  }
}

INSERT BEFORE .dynamic;
