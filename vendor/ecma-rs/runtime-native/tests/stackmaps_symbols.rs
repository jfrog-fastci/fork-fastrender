#![cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]

use core::arch::global_asm;
use runtime_native::stackmaps::STACKMAP_VERSION;

// Define a tiny but valid StackMap v3 blob (0 functions / 1 constant / 0 records).
//
// Note: runtime-native's build script injects `link/stackmaps.ld` for *tests*, which:
// - keeps `.llvm_stackmaps` under `--gc-sections`, and
// - defines `__fastr_stackmaps_start/end` at the output-section boundaries.
//
// This blob ensures the section is non-empty even in minimal environments that don't have LLVM
// tools installed (and therefore don't build the stackmap test artifact). The output section may
// still contain additional concatenated stackmap blobs from other object files.
global_asm!(
  r#"
  .section .llvm_stackmaps,"a",@progbits
  .p2align 3
  .byte 3
  .byte 0
  .short 0
  .long 0
  .long 1
  .long 0
  .quad 0x0123456789abcdef
"#
);

const MAGIC_CONST: u64 = 0x0123_4567_89ab_cdef;

#[test]
fn stackmaps_discovered_via_exported_symbols() {
  let bytes = runtime_native::stackmaps_symbols::stackmaps_bytes_from_exe();
  assert!(
    !bytes.is_empty(),
    "expected __fastr_stackmaps_start/end to cover a non-empty .llvm_stackmaps range"
  );

  // Linkers may insert alignment padding before the first blob. The runtime helper should tolerate
  // that, so check the first non-zero byte is the stackmap version.
  let version = bytes.iter().copied().find(|&b| b != 0).unwrap_or(0);
  assert_eq!(version, STACKMAP_VERSION);

  // `runtime-native`'s test build links an additional `.llvm_stackmaps` object (see `build.rs`) so
  // the symbol range may contain multiple concatenated StackMap v3 blobs.
  //
  // Assert that our tiny fixture is present somewhere in that byte range (and therefore that the
  // exported start/end symbols really delimit the `.llvm_stackmaps` output section).
  let needle = MAGIC_CONST.to_le_bytes();
  assert!(
    bytes.windows(needle.len()).any(|w| w == needle),
    "expected .llvm_stackmaps bytes to contain the injected stackmap blob (len={})",
    bytes.len()
  );

  // The general stackmaps loader should observe the same in-memory range when linker-defined
  // boundary symbols are available.
  let via_loader = runtime_native::try_load_via_linker_symbols()
    .expect("expected runtime-native tests to be linked with stackmaps.ld");
  assert_eq!(bytes, via_loader);

  let sm = runtime_native::stackmaps_symbols::stackmaps_from_exe().expect("parse stackmaps");
  assert!(
    sm.raws().iter().all(|raw| raw.version == STACKMAP_VERSION),
    "expected all parsed stackmap blobs to be v{STACKMAP_VERSION}"
  );
  assert!(
    sm.raws()
      .iter()
      .any(|raw| raw.functions.is_empty() && raw.records.is_empty() && raw.constants.as_slice() == &[MAGIC_CONST]),
    "expected parsed stackmaps to include the injected blob"
  );
}
