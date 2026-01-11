/* Linker script fragment (Linux/ELF) for retaining and exposing LLVM stack maps.
 *
 * NOTE: Prefer `runtime-native/link/stackmaps.ld`. This file is kept for
 * backwards compatibility with older build scripts and external link commands.
 *
 * See `link/stackmaps.ld` for detailed rationale and documentation.
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
/* Anchor at `.dynamic` so the writable RELRO data doesn't end up merged into the
 * executable text PT_LOAD (which would create an RWX segment). `.data` is not a
 * reliable anchor for lld: it does not create empty output sections for minimal
 * links. */
} INSERT BEFORE .dynamic;
