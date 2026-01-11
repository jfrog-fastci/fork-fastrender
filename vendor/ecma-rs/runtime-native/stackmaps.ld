/* Linker script fragment (Linux/ELF) for retaining and exposing LLVM stack maps.
 *
 * This is inserted into the linker's default script to avoid re-specifying the
 * whole layout.
 *
 * We:
 *  - keep LLVM stackmaps even under `--gc-sections`
 *  - define start/end symbols over the *loaded* section bytes
 *  - inject the section into the default linker script via `INSERT AFTER`
 *
 * We insert after `.text` (instead of the more intuitive `.rodata`) because
 * lld does not create an empty `.rodata` output section, and will error if an
 * `INSERT AFTER .rodata` fragment is used when the input objects don't
 * contribute any `.rodata` (common in minimal binaries/tests).
 *
 * Note: placing this output section "inside" the default RELRO block (e.g.
 * `INSERT BEFORE .dynamic`) would be more intuitive, but lld will error with:
 *
 *   section: .dynamic is not contiguous with other relro sections
 *
 * if we inject a custom output section between its built-in RELRO sections.
 */

SECTIONS
{
  /* Hardened location: writable during relocation, then protected by RELRO. */
  .data.rel.ro.llvm_stackmaps : ALIGN(8)
  {
    /* Preferred symbol names (shared with native-js). */
    __fastr_stackmaps_start = .;
    /* Backwards compatible aliases (older runtime-native prototypes / generic tooling). */
    __stackmaps_start = .;
    __llvm_stackmaps_start = .;
    KEEP(*(.data.rel.ro.llvm_stackmaps))
    KEEP(*(.data.rel.ro.llvm_stackmaps.*))
    /* Legacy: older LLVM/object files may still emit `.llvm_stackmaps`. */
    KEEP(*(.llvm_stackmaps))
    KEEP(*(.llvm_stackmaps.*))
    __fastr_stackmaps_end = .;
    __stackmaps_end = .;
    __llvm_stackmaps_end = .;
  }
}

INSERT AFTER .text;
