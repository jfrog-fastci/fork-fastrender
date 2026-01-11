/* Linker script fragment (Linux/ELF) for retaining and exposing LLVM stack maps
 * in **PIE/DSO-style** builds.
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
 * writable during relocation without requiring text relocations.
 *
 * lld enforces that RELRO sections are contiguous in the final layout; inserting
 * a custom, non-RELRO output section inside the RELRO range can trigger:
 *   "section: .dynamic is not contiguous with other relro sections"
 *
 * To keep the dedicated `.data.rel.ro.llvm_stackmaps` section name **and** avoid
 * lld's RELRO contiguity errors, we insert these sections immediately before
 * `.bss` (outside the RELRO range) instead of before `.dynamic`.
 *
 * Notes:
 * - `.bss` must exist for this fragment to be usable. lld can omit empty output
 *   sections like `.data` for minimal links, but it still defines `.bss`, making
 *   it a more reliable `INSERT` anchor.
 * - Avoid defining a second `.data.rel.ro` output section here: even outside
 *   the RELRO range, duplicating `.data.rel.ro` can still lead to confusing
 *   layout differences across linkers.
 * - This fragment relies on the GNU ld/LLD `INSERT` mechanism so we can inject a
 *   small section definition without replacing the entire default linker script.
 *   If your linker does not support `INSERT`, switch to GNU ld or lld.
 */

SECTIONS {
  .data.rel.ro.llvm_stackmaps : ALIGN(8) {
    . = ALIGN(8);
    __start_llvm_stackmaps = .;
    __fastr_stackmaps_start = .;
    __stackmaps_start = .;
    __llvm_stackmaps_start = .;

    KEEP(*(.data.rel.ro.llvm_stackmaps))
    KEEP(*(.data.rel.ro.llvm_stackmaps.*))

    /* StackMap v3 uses 64-bit fields and is 8-byte aligned; keep the exported
     * symbol range 8-byte aligned even if the linker inserts padding between
     * concatenated blobs. */
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
    . = ALIGN(8);
    __llvm_faultmaps_end = .;
  }
} INSERT BEFORE .bss;
