/* Linker-script fragment for LLVM stack maps (Linux / ELF).
 *
 * Background:
 * - LLVM emits stack map records into a `.llvm_stackmaps` section.
 * - With `--gc-sections`, this section is typically unreachable from `.text` and
 *   can be garbage-collected unless explicitly kept.
 *
 * This script:
 * - Forces `.llvm_stackmaps` retention via `KEEP`.
 * - Defines symbols spanning the section so the runtime can locate it without
 *   parsing ELF metadata.
 *
 * We `INSERT AFTER .data` (instead of `.rodata`) because tiny binaries linked
 * with `--gc-sections` may not contain `.rodata` at all, and lld will refuse to
 * `INSERT` relative to a section that doesn't exist.
 */

SECTIONS {
  .llvm_stackmaps : ALIGN(8) {
    __llvm_stackmaps_start = .;
    KEEP(*(.llvm_stackmaps))
    __llvm_stackmaps_end = .;
  }
} INSERT AFTER .data;

