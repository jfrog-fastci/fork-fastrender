#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use core::arch::global_asm;

// Define a tiny but valid StackMap v3 section (0 functions / 0 records) and export the same symbol
// names that `native-js`'s linker script provides in real executables.
global_asm!(
  r#"
  .section .llvm_stackmaps,"a",@progbits
  .p2align 3
  .globl __fastr_stackmaps_start
  .globl __fastr_stackmaps_end
__fastr_stackmaps_start:
  .byte 3
  .byte 0
  .short 0
  .long 0
  .long 0
  .long 0
__fastr_stackmaps_end:
"#
);

const FIXTURE: &[u8] = &[
  3, 0, // Version, Reserved0
  0, 0, // Reserved1
  0, 0, 0, 0, // NumFunctions
  0, 0, 0, 0, // NumConstants
  0, 0, 0, 0, // NumRecords
];

#[test]
fn stackmaps_discovered_via_exported_symbols() {
  let bytes = runtime_native::stackmaps_symbols::stackmaps_bytes_from_exe();
  assert_eq!(bytes, FIXTURE);

  let sm = runtime_native::stackmaps_symbols::stackmaps_from_exe().expect("parse stackmaps");
  assert_eq!(sm.raw().version, runtime_native::stackmaps::STACKMAP_VERSION);
}
