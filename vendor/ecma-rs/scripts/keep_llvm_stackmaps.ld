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
 * Placement:
 * - The output section is writable (to allow PIE/DSO relocations).
 * - We anchor the fragment at `INSERT AFTER .text;` because `.text` is always
 *   present (while `.rodata`/`.data` can be absent in minimal links and cause
 *   lld/ld errors when used as `INSERT` anchors).
 *
 *   Note: GNU ld PIE links can merge a writable stackmaps section inserted after
 *   `.text` into the executable PT_LOAD, producing an RWX segment once
 *   relocations require the segment to be writable. For GNU ld + PIE, prefer the
 *   repo's `runtime-native/link/stackmaps_gnuld.ld` fragment anchored in the
 *   RELRO/data region (used by `scripts/native_link.sh` when
 *   `ECMA_RS_NATIVE_LINKER=ld ECMA_RS_NATIVE_PIE=1`).
 * - We keep both the legacy `.llvm_*` section names and the hardened
 *   `.data.rel.ro.llvm_*` variants.
 */

SECTIONS
{
  /* Hardened location: writable during relocation, then protected by RELRO. */
  .data.rel.ro.llvm_stackmaps : ALIGN(8)
  {
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
    __stop_llvm_stackmaps = .;
    __fastr_stackmaps_end = .;
    __stackmaps_end = .;
    __llvm_stackmaps_end = .;
  }

  .data.rel.ro.llvm_faultmaps : ALIGN(8)
  {
    __llvm_faultmaps_start = .;
    KEEP(*(.data.rel.ro.llvm_faultmaps))
    KEEP(*(.data.rel.ro.llvm_faultmaps.*))
    KEEP(*(.llvm_faultmaps))
    KEEP(*(.llvm_faultmaps.*))
    __llvm_faultmaps_end = .;
  }
}

INSERT AFTER .text;
