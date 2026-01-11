#![cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]

use core::arch::global_asm;
use core::slice;

use runtime_native::stackmaps::STACKMAP_VERSION;

// Define a tiny but valid StackMap v3 blob (0 functions / 1 constant / 0 records) inside
// `.llvm_stackmaps`.
//
// Note: runtime-native's build script links tests with `link/stackmaps.ld`, which:
// - keeps `.llvm_stackmaps` under `--gc-sections`, and
// - defines `__fastr_stackmaps_start/end` to delimit the in-memory stackmaps byte range (the entire
//   output section, which may contain multiple concatenated stackmap blobs from all linked objects).
//
// We inject a known-good blob so this test can assert that the symbol-delimited range returned by
// `stackmaps_symbols::stackmaps_bytes_from_exe()` covers our bytes.
//
// We also export `__runtime_native_test_stackmaps_fixture_start/end` so the test can deterministically
// locate the injected blob and validate byte-for-byte inclusion.
global_asm!(
  r#"
  .section .llvm_stackmaps,"a",@progbits
  .p2align 3
  .globl __runtime_native_test_stackmaps_fixture_start
  .globl __runtime_native_test_stackmaps_fixture_end
__runtime_native_test_stackmaps_fixture_start:
  .byte 3
  .byte 0
  .short 0
  .long 0
  .long 1
  .long 0
  .quad 0x0123456789abcdef
__runtime_native_test_stackmaps_fixture_end:
"#
);

extern "C" {
  static __runtime_native_test_stackmaps_fixture_start: u8;
  static __runtime_native_test_stackmaps_fixture_end: u8;
}

const MAGIC_CONST: u64 = 0x0123_4567_89ab_cdef;

const FIXTURE: &[u8] = &[
  3, 0, // Version, Reserved0
  0, 0, // Reserved1
  0, 0, 0, 0, // NumFunctions
  1, 0, 0, 0, // NumConstants
  0, 0, 0, 0, // NumRecords
  0xef, 0xcd, 0xab, 0x89, 0x67, 0x45, 0x23, 0x01, // MAGIC_CONST (little endian)
];

#[test]
fn stackmaps_discovered_via_exported_symbols() {
  let bytes = runtime_native::stackmaps_symbols::stackmaps_bytes_from_exe();
  let bytes_start = bytes.as_ptr() as usize;
  let bytes_end = bytes_start + bytes.len();

  // Ensure our fixture bytes are within the exported start/end symbol range.
  let fixture_start = (&raw const __runtime_native_test_stackmaps_fixture_start) as *const u8 as usize;
  let fixture_end = (&raw const __runtime_native_test_stackmaps_fixture_end) as *const u8 as usize;
  assert!(fixture_end >= fixture_start);
  let fixture_len = fixture_end - fixture_start;
  assert_eq!(fixture_len, FIXTURE.len());

  let fixture = unsafe { slice::from_raw_parts(fixture_start as *const u8, fixture_len) };
  assert_eq!(fixture, FIXTURE);

  assert!(
    bytes_start <= fixture_start && fixture_end <= bytes_end,
    "stackmaps_bytes_from_exe did not cover the injected fixture: bytes=[{bytes_start:#x},{bytes_end:#x}) fixture=[{fixture_start:#x},{fixture_end:#x})",
  );

  let off = fixture_start - bytes_start;
  assert_eq!(&bytes[off..off + FIXTURE.len()], FIXTURE);

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
