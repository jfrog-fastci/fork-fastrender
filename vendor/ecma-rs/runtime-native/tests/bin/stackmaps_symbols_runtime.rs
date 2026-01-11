#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use runtime_native::stackmaps::StackMap;
use runtime_native::stackmaps_loader::load_llvm_stackmaps_via_symbols;

#[test]
fn loads_stackmaps_via_start_stop_symbols() {
  let bytes = load_llvm_stackmaps_via_symbols()
    .expect("linker-defined __start_llvm_stackmaps/__stop_llvm_stackmaps should be present");
  assert!(
    !bytes.is_empty(),
    "expected non-empty .llvm_stackmaps section when using the linker script"
  );

  let parsed = StackMap::parse(bytes).expect("stackmap bytes should parse");
  assert_eq!(parsed.version, 3);
}
