/* Linker script fragment for retaining and exposing LLVM stack map sections.
 *
 * Why this exists:
 * - `.llvm_stackmaps` is SHF_ALLOC but has no inbound references, so link-time
 *   section GC (`--gc-sections`) will drop it unless explicitly `KEEP`'d.
 *
 * Notes:
 * - We insert after `.text` (instead of `.rodata`) because lld will error if an
 *   `INSERT AFTER .rodata` fragment is used and the link would otherwise not
 *   create a `.rodata` output section (common in minimal test binaries).
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
