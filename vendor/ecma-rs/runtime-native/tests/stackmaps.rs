use runtime_native::stackmaps::{Location, StackMap};
use runtime_native::statepoints::{StatepointRecord, AARCH64_DWARF_REG_SP, X86_64_DWARF_REG_SP};
use runtime_native::test_util::TestRuntimeGuard;

const STACKMAP_CONST_X86_64: &[u8] = include_bytes!("fixtures/bin/stackmap_const_x86_64.bin");
const STATEPOINT_X86_64: &[u8] = include_bytes!("fixtures/bin/statepoint_x86_64.bin");
const STATEPOINT_AARCH64: &[u8] = include_bytes!("fixtures/bin/statepoint_aarch64.bin");

#[test]
fn stackmap_const_has_constant_pool_and_inline_constant() {
  let _rt = TestRuntimeGuard::new();
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
  let _rt = TestRuntimeGuard::new();
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
  let _rt = TestRuntimeGuard::new();
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
fn assert_callsite_gc_root_rbp_offsets_strict_skips_deopt_operands(patchpoint_id: u64) {
  use runtime_native::stackmaps::{CallSite, StackMapRecord, StackSize};

  // Record layout:
  //   3 header constants (callconv/flags/deopt_count)
  //   1 deopt operand (Indirect, must NOT be treated as a root)
  //   1 GC (base, derived) pair
  let rec = StackMapRecord {
    patchpoint_id,
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
    stack_size: StackSize::Known(32),
    record: &rec,
  };

  // For x86_64 with frame pointers: rbp_off = 8 - stack_size + rsp_off.
  // Deopt operand at rsp_off=0 would be -24, but must not be included.
  assert_eq!(callsite.gc_root_rbp_offsets_strict().unwrap(), vec![-8]);
}

#[cfg(target_arch = "x86_64")]
#[test]
fn callsite_reloc_pairs_skip_deopt_operands() {
  use runtime_native::stackmaps::{CallSite, StackMapRecord, StackSize};
  use runtime_native::statepoint_verify::LLVM_STATEPOINT_PATCHPOINT_ID;

  // Record layout:
  //   3 header constants (callconv/flags/deopt_count)
  //   1 deopt operand (Indirect, must NOT be treated as part of relocation pairs)
  //   1 GC (base, derived) pair
  let rec = StackMapRecord {
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
      // Relocation pair.
      Location::Indirect {
        size: 8,
        dwarf_reg: X86_64_DWARF_REG_SP,
        offset: 8,
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
    stack_size: StackSize::Known(32),
    record: &rec,
  };

  let pairs: Vec<_> = callsite.reloc_pairs().collect();
  assert_eq!(pairs.len(), 1);
  assert_eq!(
    pairs[0].base,
    Location::Indirect {
      size: 8,
      dwarf_reg: X86_64_DWARF_REG_SP,
      offset: 8
    }
  );
  assert_eq!(
    pairs[0].derived,
    Location::Indirect {
      size: 8,
      dwarf_reg: X86_64_DWARF_REG_SP,
      offset: 16
    }
  );
}

#[cfg(target_arch = "x86_64")]
#[test]
fn callsite_gc_root_rbp_offsets_strict_skips_deopt_operands() {
  assert_callsite_gc_root_rbp_offsets_strict_skips_deopt_operands(0);
}

#[cfg(target_arch = "x86_64")]
#[test]
fn callsite_gc_root_rbp_offsets_strict_skips_deopt_operands_with_nondefault_patchpoint_id() {
  assert_callsite_gc_root_rbp_offsets_strict_skips_deopt_operands(123);
}

#[cfg(target_arch = "x86_64")]
#[test]
fn callsite_reloc_pairs_do_not_require_statepoint_patchpoint_id() {
  use runtime_native::stackmaps::{CallSite, StackMapRecord, StackSize};

  // Record that looks like a statepoint (3 constant headers + even tail), but uses a non-default
  // `patchpoint_id`.
  //
  // LLVM allows overriding `"statepoint-id"` per callsite, so `CallSite::reloc_pairs` must detect
  // statepoints by their structural prefix, not by patchpoint id.
  let rec = StackMapRecord {
    patchpoint_id: 123,
    instruction_offset: 0,
    locations: vec![
      Location::Constant { size: 8, value: 0 }, // callconv
      Location::Constant { size: 8, value: 0 }, // flags
      Location::Constant { size: 8, value: 0 }, // deopt_count
      Location::Indirect {
        size: 8,
        dwarf_reg: X86_64_DWARF_REG_SP,
        offset: 8,
      },
      Location::Indirect {
        size: 8,
        dwarf_reg: X86_64_DWARF_REG_SP,
        offset: 8,
      },
    ],
    live_outs: vec![],
  };

  let callsite = CallSite {
    stack_size: StackSize::Known(32),
    record: &rec,
  };

  let pairs: Vec<_> = callsite.reloc_pairs().collect();
  assert_eq!(pairs.len(), 1);
  assert_eq!(
    pairs[0].base,
    Location::Indirect {
      size: 8,
      dwarf_reg: X86_64_DWARF_REG_SP,
      offset: 8
    }
  );
  assert_eq!(pairs[0].base, pairs[0].derived);
}

#[cfg(target_arch = "x86_64")]
#[test]
fn callsite_reloc_pairs_require_statepoint_layout() {
  use runtime_native::stackmaps::{CallSite, StackMapRecord, StackSize};

  // Record that does *not* have the LLVM statepoint header prefix (3 leading constants). Even if it
  // has an even number of pointer-bearing locations, `reloc_pairs` must treat it as a non-statepoint
  // record and yield nothing.
  let rec = StackMapRecord {
    patchpoint_id: 123,
    instruction_offset: 0,
    locations: vec![
      Location::Indirect {
        size: 8,
        dwarf_reg: X86_64_DWARF_REG_SP,
        offset: 8,
      },
      Location::Indirect {
        size: 8,
        dwarf_reg: X86_64_DWARF_REG_SP,
        offset: 8,
      },
    ],
    live_outs: vec![],
  };

  let callsite = CallSite {
    stack_size: StackSize::Known(32),
    record: &rec,
  };

  let pairs: Vec<_> = callsite.reloc_pairs().collect();
  assert!(pairs.is_empty());
}
