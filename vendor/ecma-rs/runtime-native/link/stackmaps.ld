/* Linker script fragment (Linux/ELF) for retaining and exposing LLVM stack maps.
 *
 * This is the **lld-oriented** variant. GNU ld users should prefer
 * `runtime-native/link/stackmaps_gnuld.ld` (GNU ld differs in how it assigns
 * output sections to PT_LOAD segments; see that file for details).
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
 * We place stackmaps in RELRO-friendly data so the dynamic loader can relocate
 * them for PIE/DSO binaries (writable during relocation, then protected by
 * `PT_GNU_RELRO`).
 *
 * Placement:
 * - The stackmaps payload is writable under PIE/DSO (it contains relocated
 *   function addresses). Inserting a writable section immediately after `.text`
 *   is unsafe: some linkers may merge it into the executable text PT_LOAD,
 *   forcing that segment to become RWX.
 * - lld has stricter RELRO layout rules than GNU ld and can reject custom
 *   `.data.rel.ro.*` output sections as "relro sections not contiguous".
 *   To remain lld-friendly while still using RELRO, this fragment places
 *   stackmaps/faultmaps into the standard `.data.rel.ro` output section and
 *   anchors the insertion point at `INSERT BEFORE .dynamic;`.
 *
 * Note: `.dynamic` must exist for this fragment to be usable. This is true for
 * the dynamically-linked PIE/non-PIE binaries produced by our toolchain.
 *
 * This fragment relies on the GNU ld/LLD `INSERT` mechanism so we can inject a
 * small section definition without replacing the entire default linker script.
 * If your linker does not support `INSERT`, switch to GNU ld or lld.
 */

SECTIONS {
  /* Place stackmaps inside `.data.rel.ro` (RELRO-friendly) so PIE/DSO relocations
   * do not require DT_TEXTREL, while remaining compatible with lld:
   * lld treats `.data.rel.ro` as a special RELRO section, but it can reject
   * custom `.data.rel.ro.*` output sections as "relro sections not contiguous".
   */
  .data.rel.ro : ALIGN(8) {
    . = ALIGN(8);
    __start_llvm_stackmaps = .;
    __fastr_stackmaps_start = .;
    __stackmaps_start = .;
    __llvm_stackmaps_start = .;
    KEEP(*(.data.rel.ro.llvm_stackmaps))
    KEEP(*(.data.rel.ro.llvm_stackmaps.*))
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
