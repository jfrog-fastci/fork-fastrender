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

    // Decode statepoint header + base/derived pairs.
    let sp = StatepointRecord::new(rec).unwrap();

    // Remaining entries: SP-relative Indirect locations (LLVM 18 observed output).
    for (pair_idx, pair) in sp.gc_pairs().iter().enumerate() {
      for (loc_idx, loc) in [(0, &pair.base), (1, &pair.derived)] {
        match loc {
          Location::Indirect {
            size,
            dwarf_reg,
            offset: _,
          } => {
            assert_eq!(
              *size, 8,
              "expected 8-byte pointer slots, got size={size} at pair {pair_idx} loc {loc_idx} (record #{rec_idx})"
            );
            assert_eq!(
              *dwarf_reg, sp_reg,
              "expected SP dwarf_reg={sp_reg}, got dwarf_reg={dwarf_reg} at pair {pair_idx} loc {loc_idx} (record #{rec_idx})"
            );
          }
          _ => panic!(
            "expected gc-live locations to be Indirect (SP-based), got {loc:?} at pair {pair_idx} loc {loc_idx} (record #{rec_idx})"
          ),
        }
      }
    }

    assert_eq!(
      sp.gc_pair_count(),
      (rec.locations.len() - sp.gc_pairs_start()) / 2
    );
  }

  // Evaluate one location with a fake regfile (SP=0x1000).
  let rec = &sm.records[0];
  let sp = StatepointRecord::new(rec).unwrap();
  let first_base = &sp.gc_pairs().first().unwrap().base;
  let offset = match first_base {
    Location::Indirect { offset, .. } => *offset,
    _ => unreachable!("fixtures should only use Indirect locations"),
  };

  let regs = FakeRegs {
    regs: [(sp_reg, 0x1000)].into_iter().collect(),
  };
  let slot = eval_location(first_base, &regs).unwrap();
  match slot {
    RootSlot::StackAddr(addr) => {
      let expected = (0x1000i128 + offset as i128) as u64;
      assert_eq!(addr as usize as u64, expected);
    }
    other => panic!("expected Stack slot for Indirect location, got {other:?}"),
  }
}

#[test]
fn statepoint_x86_64_layout() {
  assert_statepoint_fixture(
    include_bytes!("fixtures/bin/statepoint_x86_64.bin"),
    X86_64_DWARF_REG_SP,
  );
}

#[test]
fn statepoint_aarch64_layout() {
  assert_statepoint_fixture(
    include_bytes!("fixtures/bin/statepoint_aarch64.bin"),
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
        offset: 0,
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
fn statepoint_decoder_accepts_nonzero_flags() {
  let rec = StackMapRecord {
    patchpoint_id: 0,
    instruction_offset: 0,
    locations: vec![
      Location::Constant { size: 8, value: 0 }, // callconv
      Location::Constant { size: 8, value: 2 }, // flags (non-zero)
      Location::Constant { size: 8, value: 0 }, // deopt_count
      // One GC pair.
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

  let sp = StatepointRecord::new(&rec).unwrap();
  assert_eq!(sp.header().flags, 2);
  assert_eq!(sp.gc_pair_count(), 1);
  let pair = sp.gc_pairs().first().unwrap();
  assert_eq!(&pair.base, &rec.locations[3]);
  assert_eq!(&pair.derived, &rec.locations[4]);
}

#[test]
fn statepoint_decoder_skips_deopt_operands() {
  let rec = StackMapRecord {
    patchpoint_id: 0,
    instruction_offset: 0,
    locations: vec![
      Location::Constant { size: 8, value: 0 }, // callconv
      Location::Constant { size: 8, value: 0 }, // flags
      Location::Constant { size: 8, value: 1 }, // deopt_count = 1
      // Deopt operand location (must NOT be treated as a GC root).
      Location::Indirect {
        size: 8,
        dwarf_reg: X86_64_DWARF_REG_SP,
        offset: 1234,
      },
      // One GC pair.
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

  let sp = StatepointRecord::new(&rec).unwrap();
  assert_eq!(sp.header().deopt_count, 1);
  assert_eq!(sp.deopt_locations().len(), 1);
  assert_eq!(sp.gc_pairs_start(), 4);
  assert_eq!(sp.gc_pair_count(), 1);

  let pair = sp.gc_pairs().first().unwrap();
  assert_eq!(&pair.base, &rec.locations[4]);
  assert_eq!(&pair.derived, &rec.locations[5]);
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
