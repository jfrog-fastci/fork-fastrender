/* Linker script fragment (Linux/ELF) for retaining and exposing LLVM stack maps.
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
 * The runtime can then read the section bytes directly from memory without
 * parsing `/proc/self/exe`.
 *
 * We place the output section in `.data.rel.ro.llvm_stackmaps` so the dynamic
 * loader can relocate it for PIE binaries (writable during relocation, then
 * protected by RELRO).
 *
 * Placement:
 * - This output section is writable (to allow dynamic relocations under PIE/DSO).
 *   Inserting it immediately after `.text` is unsafe: some linkers may merge it
 *   into the executable text PT_LOAD, forcing that segment to become RWX.
 * - We anchor the fragment at `INSERT BEFORE .data;` instead. `.data` is a
 *   stable output-section name across linkers (and is present even in minimal
 *   dynamic links), and inserting before it keeps `.data.rel.ro.llvm_stackmaps`
 *   in the RELRO-covered data region (`PT_GNU_RELRO`) rather than the text
 *   segment. This is also more robust than anchoring on `.rodata`, which may be
 *   absent in minimal links (lld errors if an `INSERT` anchor does not exist).
 *
 * This fragment relies on the GNU ld/LLD `INSERT` mechanism so we can inject a
 * small section definition without replacing the entire default linker script.
 * If your linker does not support `INSERT`, switch to GNU ld or lld.
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
    __stop_llvm_stackmaps = .;
    __fastr_stackmaps_end = .;
    __stackmaps_end = .;
    __llvm_stackmaps_end = .;
  }

  /* Faultmaps are emitted by some patchpoint configurations. Keep them for
   * parity with `.llvm_stackmaps` and to avoid accidental removal under
   * `--gc-sections`. */
  .data.rel.ro.llvm_faultmaps : ALIGN(8) {
    __llvm_faultmaps_start = .;
    KEEP(*(.data.rel.ro.llvm_faultmaps))
    KEEP(*(.data.rel.ro.llvm_faultmaps.*))
    /* Legacy: older LLVM/object files may still emit `.llvm_faultmaps`. */
    KEEP(*(.llvm_faultmaps))
    KEEP(*(.llvm_faultmaps.*))
    __llvm_faultmaps_end = .;
  }
} INSERT BEFORE .data;
