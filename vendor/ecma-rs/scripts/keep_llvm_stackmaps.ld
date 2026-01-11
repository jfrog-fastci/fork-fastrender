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
  * - lld has strict `PT_GNU_RELRO` layout rules and can error if a custom linker
  *   script inserts output sections inside the RELRO region ("section: .dynamic
  *   is not contiguous with other relro sections").
  * - lld's default script ends the RELRO region with an lld-only output section
  *   named `.relro_padding`. We anchor at it (`INSERT AFTER .relro_padding;`) to
  *   keep lld's RELRO region contiguous while still placing stackmaps in a
  *   non-executable PT_LOAD segment.
  * - GNU ld has no `.relro_padding` output section; for GNU ld, prefer
  *   `runtime-native/link/stackmaps_gnuld.ld`.
  * - We keep both the legacy `.llvm_*` section names and the hardened
  *   `.data.rel.ro.llvm_*` variants.
  */

SECTIONS
{
  .data.rel.ro.llvm_stackmaps : ALIGN(8) {
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

INSERT AFTER .relro_padding;
