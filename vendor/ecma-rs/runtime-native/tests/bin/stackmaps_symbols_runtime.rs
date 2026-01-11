#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use runtime_native::stackmaps::StackMaps;
use runtime_native::stackmaps_loader::load_llvm_stackmaps_via_symbols;

#[test]
fn loads_stackmaps_via_start_stop_symbols() {
  let bytes = load_llvm_stackmaps_via_symbols()
    .expect("linker-defined __start_llvm_stackmaps/__stop_llvm_stackmaps should be present");
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
