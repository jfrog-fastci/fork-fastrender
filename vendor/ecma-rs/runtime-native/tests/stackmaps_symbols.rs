#![cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]

use core::arch::global_asm;

// Define a tiny but valid StackMap v3 blob (0 functions / 0 records).
//
// Note: runtime-native's build script injects `link/stackmaps.ld` for *tests*, which defines
// `__fastr_stackmaps_start/end` at the output-section boundaries and keeps all `.llvm_stackmaps`
// input sections under `--gc-sections`. This blob ensures the section is non-empty even in minimal
// environments that don't have LLVM tools installed (and therefore don't build the stackmap test
// artifact).
global_asm!(
  r#"
  .section .llvm_stackmaps,"a",@progbits
  .p2align 3
  .byte 3
  .byte 0
  .short 0
  .long 0
  .long 0
  .long 0
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
  assert!(!bytes.is_empty());
  // Linkers may insert alignment padding before the first blob. The runtime helper should tolerate
  // that, so check the first non-zero byte is the stackmap version.
  let version = bytes.iter().copied().find(|&b| b != 0).unwrap_or(0);
  assert_eq!(version, runtime_native::stackmaps::STACKMAP_VERSION);
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
