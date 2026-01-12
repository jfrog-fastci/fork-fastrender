/* Backwards-compatibility linker script fragment for LLVM stackmaps.
 *
 * Prefer `runtime-native/link/stackmaps.ld` (this file exists for older build
 * scripts / external link commands that refer to `runtime-native/stackmaps.ld`).
 *
 * Keep this fragment semantically equivalent to `link/stackmaps.ld`.
 *
 * Note: We intentionally duplicate the fragment instead of using:
 *
 *   INCLUDE link/stackmaps.ld
 *
 * because lld resolves `INCLUDE` paths relative to the *current working
 * directory*, not the script's directory. That makes an `INCLUDE` wrapper
 * brittle when users pass this script via an absolute `-T /abs/path/...` from a
 * different cwd.
 */

SECTIONS {
  /* Keep stackmaps/faultmaps inside the standard `.data.rel.ro` output section.
   *
   * lld is sensitive to PT_GNU_RELRO section ordering/contiguity; emitting a
   * dedicated `.data.rel.ro.llvm_stackmaps` output section can trigger:
   *   "section: .relro_padding is not contiguous with other relro sections"
   * when linking small PIE binaries.
   *
   * Appending the payloads into `.data.rel.ro` keeps them inside the existing
   * RELRO block and avoids those lld errors.
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
    KEEP(*(.llvm_faultmaps))
    KEEP(*(.llvm_faultmaps.*))
    . = ALIGN(8);
    __llvm_faultmaps_end = .;
  }
/* Insert immediately before `.bss`: it is reliably present even for minimal
 * links (lld may omit optional output sections like `.dynamic` when empty). */
} INSERT BEFORE .bss;
