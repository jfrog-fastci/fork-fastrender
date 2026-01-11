#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use runtime_native::StackMaps;

#[repr(align(8))]
struct Aligned<const N: usize>([u8; N]);

// Ensure the test binary contains a minimal, valid StackMap v3 blob so
// `StackMaps::load_self()` can locate and parse it.
#[used]
#[link_section = ".llvm_stackmaps"]
static TEST_STACKMAP_BLOB: Aligned<16> = Aligned([
  3, 0, 0, 0, // version + reserved
  0, 0, 0, 0, // num_functions
  0, 0, 0, 0, // num_constants
  0, 0, 0, 0, // num_records
]);

#[test]
fn load_self_reads_mapped_stackmaps_section() {
  let maps = StackMaps::load_self().expect("StackMaps::load_self");
  assert_eq!(maps.raw().version, 3);

  // If the current binary has any callsite records, ensure lookup works.
  if let Some(entry) = maps.callsites().first() {
    assert!(
      maps.lookup(entry.pc).is_some(),
      "expected lookup(pc) to succeed for first indexed callsite"
    );
  }
}
