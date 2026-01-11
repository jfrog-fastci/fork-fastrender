/* (Legacy) linker script fragment for retaining and exposing LLVM stack map sections.
 *
 * NOTE: Prefer:
 * - `runtime-native/link/stackmaps.ld` (lld-friendly)
 * - `runtime-native/link/stackmaps_gnuld.ld` (GNU ld PIE hardening)
 *
 * Why this exists:
 * - `.llvm_stackmaps` is SHF_ALLOC but has no inbound references, so link-time
 *   section GC (`--gc-sections`) will drop it unless explicitly `KEEP`'d.
 *
 * Notes:
 * - The stackmaps payload is writable under PIE/DSO (it contains relocated code
 *   addresses). Inserting a writable section immediately after `.text` is
 *   unsafe: some linkers will merge it into the executable PT_LOAD, creating an
 *   RWX segment.
 * - lld also has strict `PT_GNU_RELRO` layout rules and can reject custom
 *   `.data.rel.ro.*` output sections as "relro sections not contiguous".
 *   To remain lld-friendly while still using RELRO, we place stackmaps/faultmaps
 *   into the standard `.data.rel.ro` output section.
 * - We anchor at `.dynamic` (`INSERT BEFORE .dynamic;`). lld does not create
 *   empty output sections (notably `.data`), so `.dynamic` is a more reliable
 *   anchor for dynamically-linked ELF outputs.
 * - We keep both the legacy `.llvm_*` section names and the hardened
 *   `.data.rel.ro.llvm_*` variants.
 */

SECTIONS
{
  /* Keep stackmaps/faultmaps in RELRO-friendly data (writable during relocation,
   * then protected by `PT_GNU_RELRO`). */
  .data.rel.ro : ALIGN(8) {
    . = ALIGN(8);
    /* Preferred stable symbol names (shared with native-js + runtime-native). */
    __start_llvm_stackmaps = .;
    /* Project/legacy aliases used by various tools/tests. */
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
