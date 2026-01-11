/* Linker script fragment (Linux/ELF) for retaining and exposing LLVM stack maps
 * in **non-PIE** executables.
 *
 * Why a separate non-PIE fragment?
 * - PIE binaries need stackmaps in RELRO-friendly writable memory so the dynamic
 *   loader can apply relocations without `DT_TEXTREL`.
 * - For non-PIE output, stackmap relocations against code addresses are resolved
 *   at link time, so stackmaps can remain in a normal read-only section.
 *
 * Keeping stackmaps out of `.data.rel.ro` avoids lld's "relro sections not
 * contiguous" error for some injected-script layouts when stackmaps are present.
 *
 * We anchor at `INSERT AFTER .text;` because `.text` is always present, and lld
 * will error if the `INSERT` anchor does not exist.
 */
SECTIONS {
  .llvm_stackmaps : ALIGN(8) {
    . = ALIGN(8);
    __start_llvm_stackmaps = .;
    __fastr_stackmaps_start = .;
    __stackmaps_start = .;
    __llvm_stackmaps_start = .;
    KEEP(*(.llvm_stackmaps))
    KEEP(*(.llvm_stackmaps.*))
    . = ALIGN(8);
    __stop_llvm_stackmaps = .;
    __fastr_stackmaps_end = .;
    __stackmaps_end = .;
    __llvm_stackmaps_end = .;
  }

  .llvm_faultmaps : ALIGN(8) {
    . = ALIGN(8);
    __llvm_faultmaps_start = .;
    KEEP(*(.llvm_faultmaps))
    KEEP(*(.llvm_faultmaps.*))
    . = ALIGN(8);
    __llvm_faultmaps_end = .;
  }
}
INSERT AFTER .text;
