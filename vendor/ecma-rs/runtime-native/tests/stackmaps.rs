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
    assert_eq!(sp.gc_pair_count(), 2);
    for pair in sp.gc_pairs() {
      let base = &pair.base;
      let derived = &pair.derived;
      match base {
        Location::Indirect {
          size, dwarf_reg, ..
        } => {
          assert_eq!(*size, 8);
          assert_eq!(*dwarf_reg, X86_64_DWARF_REG_SP);
        }
        other => panic!("expected base to be Indirect, got {other:?}"),
      }
      assert_eq!(base, derived);
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
    assert_eq!(sp.gc_pair_count(), 2);
    for pair in sp.gc_pairs() {
      let base = &pair.base;
      let derived = &pair.derived;
      match base {
        Location::Indirect {
          size, dwarf_reg, ..
        } => {
          assert_eq!(*size, 8);
          assert_eq!(*dwarf_reg, AARCH64_DWARF_REG_SP);
        }
        other => panic!("expected base to be Indirect, got {other:?}"),
      }
      assert_eq!(base, derived);
    }
  }
}

#[cfg(target_arch = "x86_64")]
#[test]
fn callsite_gc_root_rbp_offsets_strict_skips_deopt_operands() {
  use runtime_native::stackmaps::{CallSite, StackMapRecord};
  use runtime_native::statepoint_verify::LLVM_STATEPOINT_PATCHPOINT_ID;

  // Record layout:
  //   3 header constants (callconv/flags/deopt_count)
  //   1 deopt operand (Indirect, must NOT be treated as a root)
  //   1 GC (base, derived) pair
  let rec = StackMapRecord {
    // Mark the record as a statepoint so `gc_root_rbp_offsets_strict` uses the statepoint layout
    // decoder (which skips over deopt operands before enumerating GC pairs).
    patchpoint_id: LLVM_STATEPOINT_PATCHPOINT_ID,
    instruction_offset: 0,
    locations: vec![
      Location::Constant { size: 8, value: 0 }, // callconv
      Location::Constant { size: 8, value: 0 }, // flags
      Location::Constant { size: 8, value: 1 }, // deopt_count
      // Deopt operand (must be skipped).
      Location::Indirect {
        size: 8,
        dwarf_reg: X86_64_DWARF_REG_SP,
        offset: 0,
      },
      // GC pair: Indirect [SP + 16]
      Location::Indirect {
        size: 8,
        dwarf_reg: X86_64_DWARF_REG_SP,
        offset: 16,
      },
      Location::Indirect {
        size: 8,
        dwarf_reg: X86_64_DWARF_REG_SP,
        offset: 16,
      },
    ],
    live_outs: vec![],
  };

  let callsite = CallSite {
    stack_size: 32,
    record: &rec,
  };

  // For x86_64 with frame pointers: rbp_off = 8 - stack_size + rsp_off.
  // Deopt operand at rsp_off=0 would be -24, but must not be included.
  assert_eq!(callsite.gc_root_rbp_offsets_strict().unwrap(), vec![-8]);
}
