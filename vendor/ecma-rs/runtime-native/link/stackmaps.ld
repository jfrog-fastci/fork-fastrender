/* Linker script fragment (Linux/ELF) for retaining and exposing LLVM stack maps
 * in **PIE/DSO-style** builds (lld-oriented).
 *
 * Why a linker script at all?
 * - LLVM emits stackmap metadata into a `.llvm_stackmaps` section.
 * - GNU ld/LLD only auto-define `__start_<section>` / `__stop_<section>` for
 *   section names *without* a leading dot, so `.llvm_stackmaps` does not get
 *   usable start/stop symbols.
 *
 * This fragment keeps stackmaps under `--gc-sections` and defines stable
 * in-process boundary symbols:
 *   - `__start_llvm_stackmaps`
 *   - `__stop_llvm_stackmaps`
 *
 * It also defines aliases:
 *   - `__stackmaps_{start,end}` (generic alias)
 *   - `__fastr_stackmaps_{start,end}` / `__llvm_stackmaps_{start,end}` (legacy)
 *
 * `native-js` rewrites `.llvm_stackmaps` / `.llvm_faultmaps` input sections to
 * `.data.rel.ro.llvm_stackmaps` / `.data.rel.ro.llvm_faultmaps` (via
 * `llvm-objcopy --rename-section ...`) before linking. This makes the sections
 * writable during relocation without requiring text relocations (DT_TEXTREL).
 *
 * RELRO + lld constraints:
 * - lld enforces that RELRO sections are contiguous in the final layout; custom
 *   output sections placed within the RELRO range can trigger:
 *     "section: .dynamic is not contiguous with other relro sections"
 * - To remain lld-friendly while still keeping stackmaps under `PT_GNU_RELRO`,
 *   we place stackmaps/faultmaps into the standard `.data.rel.ro` output
 *   section, anchored before `.dynamic`.
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
    /* Legacy: older LLVM/object files may still emit `.llvm_stackmaps`. */
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

    /* Faultmaps are emitted by some patchpoint configurations. Keep them for
     * parity with `.llvm_stackmaps` and to avoid accidental removal under
     * `--gc-sections`. */
    __llvm_faultmaps_start = .;
    KEEP(*(.data.rel.ro.llvm_faultmaps))
    KEEP(*(.data.rel.ro.llvm_faultmaps.*))
    /* Legacy: older LLVM/object files may still emit `.llvm_faultmaps`. */
    KEEP(*(.llvm_faultmaps))
    KEEP(*(.llvm_faultmaps.*))
    . = ALIGN(8);
    __llvm_faultmaps_end = .;
  }
} INSERT BEFORE .dynamic;
