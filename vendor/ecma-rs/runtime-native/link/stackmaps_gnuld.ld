/* GNU ld-only linker script fragment (Linux/ELF) for retaining and exposing LLVM stack maps.
 *
 * This is the GNU ld counterpart to `runtime-native/link/stackmaps.ld`.
 *
 * Why a separate fragment?
 * - Prefer `runtime-native/link/stackmaps.ld` for lld: it anchors at `INSERT BEFORE .dynamic;` and
 *   appends stackmaps into the standard `.data.rel.ro` output section to satisfy lld's "RELRO
 *   contiguity" enforcement.
 * - GNU ld differs in how it assigns output sections to PT_LOAD segments. For GNU ld we keep
 *   stackmaps/faultmaps in dedicated `.data.rel.ro.llvm_*` output sections so tooling can locate
 *   them by section name even when symbol tables are stripped (`scripts/native_link.sh` selects
 *   this fragment automatically when `ECMA_RS_NATIVE_LINKER=ld` and `ECMA_RS_NATIVE_PIE=1`).
 * - Anchoring a writable `.data.rel.ro.*` output section after `.text` is unsafe on GNU ld: it may
 *   place the section in the same PT_LOAD segment as `.text`, forcing that segment to become RWX.
 *   Insert before `.dynamic` instead so the payload stays in the data/RELRO region.
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
