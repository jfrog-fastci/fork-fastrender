use runtime_native::stackmaps::{Location, StackMap};
use runtime_native::statepoints::{
  eval_location, RegFile, RootSlot, StatepointRecord, AARCH64_DWARF_REG_SP,
  LLVM18_STATEPOINT_HEADER_CONSTANTS, X86_64_DWARF_REG_SP,
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
  assert_eq!(sm.records.len(), 1);
  let rec = &sm.records[0];

  assert!(
    rec.locations.len() >= LLVM18_STATEPOINT_HEADER_CONSTANTS,
    "need at least {LLVM18_STATEPOINT_HEADER_CONSTANTS} locations, got {}",
    rec.locations.len()
  );

  // 3 leading constants.
  for i in 0..LLVM18_STATEPOINT_HEADER_CONSTANTS {
    assert!(
      matches!(rec.locations[i], Location::Constant { .. } | Location::ConstIndex { .. }),
      "expected locations[{i}] to be a constant header, got {:?}",
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
          "expected 8-byte pointer slots, got size={size} at remaining index {i}"
        );
        assert_eq!(
          dwarf_reg, sp_reg,
          "expected SP dwarf_reg={sp_reg}, got dwarf_reg={dwarf_reg} at remaining index {i}"
        );
      }
      _ => panic!(
        "expected remaining locations to be Indirect (SP-based), got {loc:?} at remaining index {i}"
      ),
    }
  }

  // Decode statepoint base/derived pairs.
  let sp = StatepointRecord::new(rec).unwrap();
  assert_eq!(
    sp.gc_pairs().len(),
    (rec.locations.len() - LLVM18_STATEPOINT_HEADER_CONSTANTS) / 2
  );

  // Evaluate one location with a fake regfile (SP=0x1000).
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

