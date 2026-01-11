#![cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]

use runtime_native::stackmaps::StackMaps;
use runtime_native::stackmaps_loader::load_llvm_stackmaps_via_symbols;

#[repr(align(8))]
struct Aligned<const N: usize>([u8; N]);

// Unit tests for stackmap discovery shouldn't depend on LLVM tools being present to generate
// `stackmap_test.o` in build.rs. Link a minimal StackMap v3 header into `.llvm_stackmaps` so the
// linker-script exported `__start_llvm_stackmaps` / `__stop_llvm_stackmaps` range is non-empty even
// in tool-less environments.
#[used]
#[link_section = ".llvm_stackmaps"]
static TEST_STACKMAP_BLOB: Aligned<16> = Aligned([
  3, 0, 0, 0, // version + reserved
  0, 0, 0, 0, // num_functions
  0, 0, 0, 0, // num_constants
  0, 0, 0, 0, // num_records
]);

#[test]
fn loads_stackmaps_via_linker_symbols() {
  let bytes = load_llvm_stackmaps_via_symbols()
    .expect("linker-defined __stackmaps_start/__stackmaps_end should be present");
  assert!(
    !bytes.is_empty(),
    "expected non-empty .llvm_stackmaps section when using the linker script"
  );

  // A final linked binary may contain multiple StackMap v3 blobs concatenated by the linker
  // (one per input object). Use the multi-blob parser + index builder.
  let parsed = StackMaps::parse(bytes).expect("stackmap bytes should parse");
  assert!(
    !parsed.raws().is_empty(),
    "expected at least one parsed StackMap blob"
  );
  assert!(parsed.raws().iter().all(|sm| sm.version == 3));
}
