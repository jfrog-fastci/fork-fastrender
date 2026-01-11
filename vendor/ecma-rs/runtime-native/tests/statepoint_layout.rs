use runtime_native::stackmaps::{Location, StackMap, StackMapRecord};
use runtime_native::statepoints::{
  eval_location, RegFile, RootSlot, StatepointError, StatepointRecord, AARCH64_DWARF_REG_SP,
  LLVM18_STATEPOINT_HEADER_CONSTANTS, X86_64_DWARF_REG_FP, X86_64_DWARF_REG_SP,
};

struct FakeRegs {
  regs: std::collections::HashMap<u16, u64>,
}

impl RegFile for FakeRegs {
  fn get(&self, dwarf_reg: u16) -> Option<u64> {
    self.regs.get(&dwarf_reg).copied()
  }
}

fn assert_statepoint_fixture(bytes: &[u8], sp_reg: u16) {
  let sm = StackMap::parse(bytes).unwrap();
  assert!(
    !sm.records.is_empty(),
    "expected fixture to contain at least one stackmap record"
  );

  for (rec_idx, rec) in sm.records.iter().enumerate() {
    assert!(
      rec.locations.len() >= LLVM18_STATEPOINT_HEADER_CONSTANTS,
      "need at least {LLVM18_STATEPOINT_HEADER_CONSTANTS} locations, got {} (record #{rec_idx})",
      rec.locations.len()
    );

    // 3 leading constants.
    for i in 0..LLVM18_STATEPOINT_HEADER_CONSTANTS {
      assert!(
        matches!(rec.locations[i], Location::Constant { .. } | Location::ConstIndex { .. }),
        "expected locations[{i}] to be a constant header, got {:?} (record #{rec_idx})",
        rec.locations[i]
      );
    }

    // Remaining entries: SP-relative Indirect locations (LLVM 18 observed output).
    for (i, loc) in rec.locations[LLVM18_STATEPOINT_HEADER_CONSTANTS..].iter().enumerate() {
      match *loc {
        Location::Indirect {
          size,
          dwarf_reg,
          offset: _,
        } => {
          assert_eq!(
            size, 8,
            "expected 8-byte pointer slots, got size={size} at remaining index {i} (record #{rec_idx})"
          );
          assert_eq!(
            dwarf_reg, sp_reg,
            "expected SP dwarf_reg={sp_reg}, got dwarf_reg={dwarf_reg} at remaining index {i} (record #{rec_idx})"
          );
        }
        _ => panic!(
          "expected remaining locations to be Indirect (SP-based), got {loc:?} at remaining index {i} (record #{rec_idx})"
        ),
      }
    }

    // Decode statepoint base/derived pairs.
    let sp = StatepointRecord::new(rec).unwrap();
    assert_eq!(
      sp.gc_pairs().len(),
      (rec.locations.len() - LLVM18_STATEPOINT_HEADER_CONSTANTS) / 2
    );
  }

  // Evaluate one location with a fake regfile (SP=0x1000).
  let rec = &sm.records[0];
  let sp = StatepointRecord::new(rec).unwrap();
  let first_base = sp.gc_pairs()[0].base;
  let offset = match *first_base {
    Location::Indirect { offset, .. } => offset,
    _ => unreachable!("fixtures should only use Indirect locations"),
  };

  let regs = FakeRegs {
    regs: [(sp_reg, 0x1000)].into_iter().collect(),
  };
  let slot = eval_location(first_base, &regs).unwrap();
  match slot {
    RootSlot::Stack { addr } => {
      let expected = (0x1000i128 + offset as i128) as u64;
      assert_eq!(addr as usize as u64, expected);
    }
    other => panic!("expected Stack slot for Indirect location, got {other:?}"),
  }
}

#[test]
fn statepoint_x86_64_layout() {
  assert_statepoint_fixture(
    include_bytes!("fixtures/statepoint_x86_64.bin"),
    X86_64_DWARF_REG_SP,
  );
}

#[test]
fn statepoint_aarch64_layout() {
  assert_statepoint_fixture(
    include_bytes!("fixtures/statepoint_aarch64.bin"),
    AARCH64_DWARF_REG_SP,
  );
}

#[test]
fn eval_direct_location_is_immediate_value() {
  let loc = Location::Direct {
    size: 8,
    dwarf_reg: X86_64_DWARF_REG_FP,
    offset: -8,
  };

  let regs = FakeRegs {
    regs: [(X86_64_DWARF_REG_FP, 0x1000)].into_iter().collect(),
  };
  let slot = eval_location(&loc, &regs).unwrap();
  assert_eq!(slot, RootSlot::Const { value: 0x0ff8 });
}

#[test]
fn statepoint_record_rejects_odd_tail_len() {
  let rec = StackMapRecord {
    patchpoint_id: 0,
    instruction_offset: 0,
    locations: vec![
      Location::Constant { size: 8, value: 0 },
      Location::Constant { size: 8, value: 0 },
      Location::Constant { size: 8, value: 0 },
      // Odd tail length (should be base+derived pairs).
      Location::Indirect {
        size: 8,
        dwarf_reg: X86_64_DWARF_REG_SP,
        offset: 0,
      },
    ],
    live_outs: vec![],
  };

  assert!(matches!(
    StatepointRecord::new(&rec),
    Err(StatepointError::InvalidLayout { .. })
  ));
}

#[test]
fn statepoint_record_rejects_nonconstant_header() {
  let rec = StackMapRecord {
    patchpoint_id: 0,
    instruction_offset: 0,
    locations: vec![
      Location::Register {
        size: 8,
        dwarf_reg: X86_64_DWARF_REG_SP,
      },
      Location::Constant { size: 8, value: 0 },
      Location::Constant { size: 8, value: 0 },
      Location::Indirect {
        size: 8,
        dwarf_reg: X86_64_DWARF_REG_SP,
        offset: 0,
      },
      Location::Indirect {
        size: 8,
        dwarf_reg: X86_64_DWARF_REG_SP,
        offset: 0,
      },
    ],
    live_outs: vec![],
  };

  assert!(matches!(
    StatepointRecord::new(&rec),
    Err(StatepointError::NonConstantHeader { index: 0 })
  ));
}

#[test]
fn eval_indirect_missing_reg_errors() {
  let loc = Location::Indirect {
    size: 8,
    dwarf_reg: X86_64_DWARF_REG_SP,
    offset: 0,
  };
  let regs = FakeRegs {
    regs: Default::default(),
  };
  assert!(matches!(
    eval_location(&loc, &regs),
    Err(StatepointError::MissingRegister {
      dwarf_reg: X86_64_DWARF_REG_SP
    })
  ));
}

#[test]
fn eval_indirect_overflow_errors() {
  let loc = Location::Indirect {
    size: 8,
    dwarf_reg: X86_64_DWARF_REG_SP,
    offset: 1,
  };
  let regs = FakeRegs {
    regs: [(X86_64_DWARF_REG_SP, u64::MAX)].into_iter().collect(),
  };
  assert!(matches!(
    eval_location(&loc, &regs),
    Err(StatepointError::AddressOverflow { .. })
  ));
}
