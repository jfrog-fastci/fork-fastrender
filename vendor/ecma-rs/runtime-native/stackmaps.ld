/* Linker script fragment (Linux/ELF) for retaining and exposing LLVM stack maps.
 *
 * This is inserted into the linker's default script to avoid re-specifying the
 * whole layout.
 *
 * We:
 *  - keep `.llvm_stackmaps` even under `--gc-sections`
 *  - define start/end symbols over the *loaded* section bytes
 *  - place the section in the read-only segment by inserting after `.rodata`
 */

SECTIONS
{
  .llvm_stackmaps : ALIGN(8)
  {
    __llvm_stackmaps_start = .;
    KEEP(*(.llvm_stackmaps))
    KEEP(*(.llvm_stackmaps.*))
    __llvm_stackmaps_end = .;
  }
}

INSERT AFTER .rodata;

