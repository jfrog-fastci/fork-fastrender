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
 * We insert after `.text` (instead of `.rodata`) because lld does not create an
 * empty `.rodata` output section, and will error if an `INSERT AFTER .rodata`
 * fragment is used when the input objects don't contribute any `.rodata`
 * (common in minimal binaries/tests).
 *
 * Note: GNU ld assigns an `INSERT AFTER .text` output section to the same LOAD
 * segment as `.text` by default. If stackmaps are made writable to avoid
 * `DT_TEXTREL` in PIE mode, that can produce an RWX segment. For GNU ld PIE
 * builds, prefer `runtime-native/link/stackmaps_gnuld.ld` (or use
 * `scripts/native_link.sh`, which selects the appropriate fragment).
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
} INSERT AFTER .text;
