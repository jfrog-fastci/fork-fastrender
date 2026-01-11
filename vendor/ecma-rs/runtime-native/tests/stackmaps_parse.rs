use runtime_native::stackmaps::{StackMap, StackMaps};

#[test]
fn parse_llvm_stackmaps_v3_fixture() {
  let bytes = include_bytes!("fixtures/llvm_stackmaps_v3.bin");
  let raw = StackMap::parse(bytes).expect("parse fixture");

  assert_eq!(raw.version, 3);
  assert!(!raw.functions.is_empty());
  assert!(!raw.records.is_empty());

  let f = &raw.functions[0];
  let rec = &raw.records[0];
  let pc = f.address + rec.instruction_offset as u64;

  let registry = StackMaps::parse(bytes).expect("build registry");
  assert!(registry.lookup(pc).is_some());
}
