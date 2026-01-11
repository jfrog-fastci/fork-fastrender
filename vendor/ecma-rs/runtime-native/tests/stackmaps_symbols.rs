#![cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]

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
  // Simulate the output-section-level alignment padding that linkers can insert
  // before the first input section payload.
  .byte 0,0,0,0,0,0,0,0
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
  // Leading padding (8 bytes).
  0, 0, 0, 0, 0, 0, 0, 0,
  3, 0, // Version, Reserved0
  0, 0, // Reserved1
  0, 0, 0, 0, // NumFunctions
  0, 0, 0, 0, // NumConstants
  0, 0, 0, 0, // NumRecords
];

#[test]
fn stackmaps_discovered_via_exported_symbols() {
  let bytes = runtime_native::stackmaps_symbols::stackmaps_bytes_from_exe();
  // `runtime-native`'s test build links an additional `.llvm_stackmaps` object (see `build.rs`) so
  // the symbol range may contain multiple concatenated StackMap v3 blobs.
  //
  // Assert that our tiny fixture is present somewhere in that byte range (and therefore that the
  // exported start/end symbols really delimit the `.llvm_stackmaps` output section).
  assert!(
    bytes.windows(FIXTURE.len()).any(|w| w == FIXTURE),
    "expected .llvm_stackmaps bytes to contain the test fixture (len={})",
    bytes.len()
  );

  let sm = runtime_native::stackmaps_symbols::stackmaps_from_exe().expect("parse stackmaps");
  assert_eq!(sm.raw().version, runtime_native::stackmaps::STACKMAP_VERSION);
}
