use runtime_native::stackmaps::{Location, StackMap};
use runtime_native::statepoints::{eval_location, RegFile, RootSlot};

struct EmptyRegs;

impl RegFile for EmptyRegs {
  fn get(&self, _dwarf_reg: u16) -> Option<u64> {
    None
  }
}

#[test]
fn parses_constindex_location() {
  let bytes = include_bytes!("fixtures/stackmap_constindex_x86_64.bin");
  let sm = StackMap::parse(bytes).unwrap();

  assert_eq!(sm.constants, vec![1234567890123456789]);
  assert_eq!(sm.records.len(), 1);
  let rec = &sm.records[0];
  assert_eq!(rec.locations.len(), 1);

  assert_eq!(
    rec.locations[0],
    Location::ConstIndex {
      size: 8,
      index: 0,
      value: 1234567890123456789,
    }
  );

  let slot = eval_location(&rec.locations[0], &EmptyRegs).unwrap();
  assert_eq!(
    slot,
    RootSlot::Const {
      value: 1234567890123456789
    }
  );
}

