SECTIONS {
  /*
   * LLVM statepoints emit GC stack maps into `.llvm_stackmaps`. The section is
   * SHF_ALLOC but has no inbound code references, so `--gc-sections` drops it
   * unless we explicitly KEEP it.
   *
   * We also KEEP `.llvm_faultmaps` (used by some patchpoint configurations).
   *
   * NOTE: This script relies on GNU ld's `INSERT` extension so we can augment
   * the default linker script without copying it.
   */
  .llvm_stackmaps : ALIGN(8) {
    __llvm_stackmaps_start = .;
    KEEP(*(.llvm_stackmaps))
    KEEP(*(.llvm_stackmaps.*))
    __llvm_stackmaps_end = .;
  }

  .llvm_faultmaps : ALIGN(8) {
    __llvm_faultmaps_start = .;
    KEEP(*(.llvm_faultmaps))
    KEEP(*(.llvm_faultmaps.*))
    __llvm_faultmaps_end = .;
  }
} INSERT AFTER .rodata;
