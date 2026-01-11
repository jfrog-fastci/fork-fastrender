/* Backwards-compatibility entrypoint for `link/stackmaps.ld`.
 *
 * NOTE: Prefer `runtime-native/link/stackmaps.ld`. This file is kept for
 * backwards compatibility with older build scripts and external link commands.
 *
 * It exports the same boundary symbols as `link/stackmaps.ld`, but the exact
 * section placement may differ; prefer `link/stackmaps.ld` for the canonical
 * lld-oriented PIE/DSO behavior.
 *
 * Keep this file as a thin wrapper so it stays in sync with the real script.
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
    /* Legacy: older LLVM/object files may still emit `.llvm_stackmaps`. */
    KEEP(*(.llvm_stackmaps))
    KEEP(*(.llvm_stackmaps.*))
    . = ALIGN(8);
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
    . = ALIGN(8);
    __llvm_faultmaps_end = .;
  }
/* Keep the insertion outside lld's RELRO range: lld enforces that RELRO sections
 * remain contiguous, and injecting custom `.data.rel.ro.*` output sections
 * inside RELRO can trigger:
 *   "section: .got is not contiguous with other relro sections"
 */
} INSERT BEFORE .bss;
