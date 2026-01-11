/* GNU ld-only linker script fragment (Linux/ELF) for retaining and exposing LLVM stack maps.
 *
 * This is the GNU ld counterpart to `runtime-native/link/stackmaps.ld`.
 *
 * Why a separate fragment?
 * - `runtime-native/link/stackmaps.ld` is optimized for lld: it inserts the
 *   `.data.rel.ro.llvm_stackmaps` / `.data.rel.ro.llvm_faultmaps` sections
 *   immediately before `.bss` to avoid lld's RELRO contiguity errors.
 * - GNU ld differs in how it assigns output sections to PT_LOAD segments. For GNU ld we keep
 *   stackmaps/faultmaps in dedicated `.data.rel.ro.llvm_*` output sections and insert before
 *   `.dynamic` so the payload stays in the data/RELRO region (anchoring a writable `.data.rel.ro.*`
 *   section after `.text` can cause GNU ld to create RWX segments).
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
    . = ALIGN(8);
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
    . = ALIGN(8);
    __llvm_faultmaps_end = .;
  }
}

INSERT BEFORE .dynamic;
