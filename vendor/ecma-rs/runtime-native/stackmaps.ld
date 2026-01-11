/* Linker script fragment (Linux/ELF) for retaining and exposing LLVM stack maps.
 *
 * This is inserted into the linker's default script to avoid re-specifying the
 * whole layout.
 *
 * We:
 *  - keep `.llvm_stackmaps` even under `--gc-sections`
 *  - define start/end symbols over the *loaded* section bytes
 *  - inject the section into the default linker script via `INSERT AFTER`
 *
 * We insert after `.text` (instead of the more intuitive `.rodata`) because
 * lld does not create an empty `.rodata` output section, and will error if an
 * `INSERT AFTER .rodata` fragment is used when the input objects don't
 * contribute any `.rodata` (common in minimal binaries/tests).
 */

SECTIONS
{
  .llvm_stackmaps : ALIGN(8)
  {
    __fastr_stackmaps_start = .;
    __llvm_stackmaps_start = .;
    KEEP(*(.llvm_stackmaps))
    KEEP(*(.llvm_stackmaps.*))
    __fastr_stackmaps_end = .;
    __llvm_stackmaps_end = .;
  }
}

INSERT AFTER .text;
