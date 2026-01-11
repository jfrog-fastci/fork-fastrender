/* GNU ld-only linker script fragment (Linux/ELF) for retaining and exposing LLVM stack maps.
 *
 * This is the GNU ld counterpart to `runtime-native/link/stackmaps.ld`.
 *
 * Why a separate fragment?
 * - lld requires `INSERT AFTER .text` (it errors if anchoring on an absent
 *   `.rodata` output section).
 * - GNU ld assigns an `INSERT AFTER .text` output section to the same LOAD
 *   segment as `.text` by default. If `.llvm_stackmaps` needs to be writable
 *   during PIE relocation (to avoid `DT_TEXTREL`), that segment becomes RWX.
 *
 * To avoid RWX on GNU ld, we insert the output section into the RELRO/data
 * region instead (before `.dynamic`). This keeps the section in a non-executable
 * segment while still allowing runtime relocations for PIE binaries.
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

