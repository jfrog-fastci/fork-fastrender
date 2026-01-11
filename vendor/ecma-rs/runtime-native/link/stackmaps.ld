/* Linker script fragment (Linux/ELF) for retaining and exposing LLVM stack maps
 * in **PIE/DSO-style** builds (lld-oriented).
 *
 * - LLVM emits stackmap metadata into `.llvm_stackmaps` (and sometimes `.llvm_faultmaps`).
 * - For PIE/DSO output we often rename those input sections to `.data.rel.ro.llvm_*` so the dynamic
 *   loader can apply relocations without `DT_TEXTREL`.
 * - GNU ld/LLD only auto-define `__start_<section>` / `__stop_<section>` for section names without
 *   a leading dot, so `.llvm_stackmaps` does not get usable boundary symbols by default.
 *
 * This fragment:
 * - Keeps stackmaps/faultmaps under `--gc-sections`, and
 * - Defines stable in-process symbols:
 *     __start_llvm_stackmaps / __stop_llvm_stackmaps
 *     __stackmaps_start / __stackmaps_end (generic aliases)
 *     __fastr_stackmaps_* / __llvm_stackmaps_* (legacy aliases)
 *
 * PIE note:
 * LLVM stackmaps contain code pointers, which become runtime relocations under
 * PIE. lld rejects text relocations, so stackmaps must live in writable memory
 * during relocation. In our pipeline, `.llvm_stackmaps` / `.llvm_faultmaps`
 * input sections may be rewritten to `.data.rel.ro.llvm_stackmaps` /
 * `.data.rel.ro.llvm_faultmaps` (via `llvm-objcopy --rename-section ...`) so
 * they naturally land in RELRO-friendly memory.
 *
 * RELRO + lld constraints:
 * - For PIE/DSO links we want stackmaps/faultmaps to be writable while the
 *   dynamic loader applies relocations, then protected by RELRO (PT_GNU_RELRO).
 * - lld is sensitive to RELRO section ordering/contiguity. The most robust
 *   approach is to append the stackmaps/faultmaps payloads into the standard
 *   `.data.rel.ro` output section (instead of creating dedicated output section
 *   names) so they stay inside the existing RELRO block.
 *
 * This fragment relies on the GNU ld/LLD `INSERT` mechanism so we can inject a
 * small section definition without replacing the entire default linker script.
 *
 * Use:
 * - non-PIE output: `runtime-native/link/stackmaps_nopie.ld`
 * - GNU ld PIE:     `runtime-native/link/stackmaps_gnuld.ld`
 */

SECTIONS {
  /* Keep stackmaps/faultmaps in RELRO-friendly data (writable during relocation,
   * then protected by `PT_GNU_RELRO`). */
  .data.rel.ro : ALIGN(8) {
    . = ALIGN(8);
    __start_llvm_stackmaps = .;
    __fastr_stackmaps_start = .;
    __stackmaps_start = .;
    __llvm_stackmaps_start = .;
    KEEP(*(.data.rel.ro.llvm_stackmaps))
    KEEP(*(.data.rel.ro.llvm_stackmaps.*))
    /* Legacy: some objects/tools still use the `.llvm_stackmaps` name. */
    KEEP(*(.llvm_stackmaps))
    KEEP(*(.llvm_stackmaps.*))
    /* StackMap v3 uses 64-bit fields and is 8-byte aligned; keep the exported
     * symbol range 8-byte aligned even if the linker inserts padding between
     * concatenated blobs. */
    . = ALIGN(8);
    __stop_llvm_stackmaps = .;
    __fastr_stackmaps_end = .;
    __stackmaps_end = .;
    __llvm_stackmaps_end = .;
    __llvm_faultmaps_start = .;
    KEEP(*(.data.rel.ro.llvm_faultmaps))
    KEEP(*(.data.rel.ro.llvm_faultmaps.*))
    /* Legacy: some objects/tools still use the `.llvm_faultmaps` name. */
    KEEP(*(.llvm_faultmaps))
    KEEP(*(.llvm_faultmaps.*))
    . = ALIGN(8);
    __llvm_faultmaps_end = .;
  }
/* Anchor at `.dynamic`: it is present in PIE/DSO links (even `-nostdlib`
 * minimal links), and is inside the RELRO block so stackmaps stay covered by
 * PT_GNU_RELRO. */
} INSERT BEFORE .dynamic;
