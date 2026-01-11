use runtime_native::stackmaps::{Location, StackMap};
use runtime_native::statepoints::{StatepointRecord, AARCH64_DWARF_REG_SP, X86_64_DWARF_REG_SP};

const STACKMAP_CONST_X86_64: &[u8] = include_bytes!("fixtures/bin/stackmap_const_x86_64.bin");
const STATEPOINT_X86_64: &[u8] = include_bytes!("fixtures/bin/statepoint_x86_64.bin");
const STATEPOINT_AARCH64: &[u8] = include_bytes!("fixtures/bin/statepoint_aarch64.bin");

#[test]
fn stackmap_const_has_constant_pool_and_inline_constant() {
  let stackmap = StackMap::parse(STACKMAP_CONST_X86_64).unwrap();
  assert_eq!(stackmap.version, 3);
  assert_eq!(stackmap.functions.len(), 1);
  assert_eq!(stackmap.constants, vec![0x1122_3344_5566_7788]);
  assert_eq!(stackmap.records.len(), 1);

  let record = &stackmap.records[0];
  assert_eq!(record.patchpoint_id, 1);
  assert_eq!(record.locations.len(), 2);

  match record.locations[0] {
    Location::ConstIndex { size, index, value } => {
      assert_eq!(size, 8);
      assert_eq!(index, 0);
      assert_eq!(value, 0x1122_3344_5566_7788);
    }
    ref other => panic!("expected locations[0] to be ConstIndex, got {other:?}"),
  }

  match record.locations[1] {
    Location::Constant { size, value } => {
      assert_eq!(size, 8);
      assert_eq!(value, 7);
    }
    ref other => panic!("expected locations[1] to be Constant, got {other:?}"),
  }
}

#[test]
fn statepoint_stackmap_x86_64_has_two_gc_live_pointers() {
  let stackmap = StackMap::parse(STATEPOINT_X86_64).unwrap();
  assert_eq!(stackmap.version, 3);
  assert_eq!(stackmap.constants.len(), 0);
  assert_eq!(stackmap.records.len(), 2);

  for record in &stackmap.records {
    assert_eq!(record.patchpoint_id, 0xABCD_EF00);
    assert_eq!(record.locations.len(), 7);

    let sp = StatepointRecord::new(record).unwrap();
    assert_eq!(sp.gc_pairs().len(), 2);
    for pair in sp.gc_pairs() {
      match pair.base {
        Location::Indirect {
          size, dwarf_reg, ..
        } => {
          assert_eq!(*size, 8);
          assert_eq!(*dwarf_reg, X86_64_DWARF_REG_SP);
        }
        other => panic!("expected base to be Indirect, got {other:?}"),
      }
      assert_eq!(pair.base, pair.derived);
    }
  }
}

#[test]
fn statepoint_stackmap_aarch64_has_two_gc_live_pointers() {
  let stackmap = StackMap::parse(STATEPOINT_AARCH64).unwrap();
  assert_eq!(stackmap.version, 3);
  assert_eq!(stackmap.constants.len(), 0);
  assert_eq!(stackmap.records.len(), 2);

  for record in &stackmap.records {
    assert_eq!(record.patchpoint_id, 0xABCD_EF00);
    assert_eq!(record.locations.len(), 7);

    let sp = StatepointRecord::new(record).unwrap();
    assert_eq!(sp.gc_pairs().len(), 2);
    for pair in sp.gc_pairs() {
      match pair.base {
        Location::Indirect {
          size, dwarf_reg, ..
        } => {
          assert_eq!(*size, 8);
          assert_eq!(*dwarf_reg, AARCH64_DWARF_REG_SP);
        }
        other => panic!("expected base to be Indirect, got {other:?}"),
      }
      assert_eq!(pair.base, pair.derived);
    }
  }
}
