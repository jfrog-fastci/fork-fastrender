use runtime_native::stackmaps::{Location, StackMap};
use runtime_native::statepoints::{eval_location, RegFile, RootSlot};

struct MapRegs(std::collections::HashMap<u16, u64>);

impl RegFile for MapRegs {
  fn get(&self, dwarf_reg: u16) -> Option<u64> {
    self.0.get(&dwarf_reg).copied()
  }
}

#[test]
fn parses_register_location() {
  let bytes = include_bytes!("fixtures/bin/stackmap_register_x86_64.bin");
  let sm = StackMap::parse(bytes).unwrap();
  assert_eq!(sm.records.len(), 1);

  let rec = &sm.records[0];
  assert_eq!(rec.locations.len(), 1);
  let loc = &rec.locations[0];

  let dwarf_reg = match *loc {
    Location::Register { size, dwarf_reg, .. } => {
      assert_eq!(size, 8);
      dwarf_reg
    }
    _ => panic!("expected Register location, got {loc:?}"),
  };

  let slot = eval_location(loc, &MapRegs(Default::default())).unwrap();
  assert_eq!(slot, RootSlot::Reg { dwarf_reg });
}

#[test]
fn parses_direct_location() {
  let bytes = include_bytes!("fixtures/bin/stackmap_direct_x86_64.bin");
  let sm = StackMap::parse(bytes).unwrap();
  assert_eq!(sm.records.len(), 1);

  let rec = &sm.records[0];
  assert_eq!(rec.locations.len(), 1);
  let loc = &rec.locations[0];

  let (dwarf_reg, offset) = match *loc {
    Location::Direct {
      size,
      dwarf_reg,
      offset,
    } => {
      assert_eq!(size, 8);
      (dwarf_reg, offset)
    }
    _ => panic!("expected Direct location, got {loc:?}"),
  };

  let regs = MapRegs([(dwarf_reg, 0x1000)].into_iter().collect());
  let slot = eval_location(loc, &regs).unwrap();

  let expected = (0x1000i128 + offset as i128) as u64;
  assert_eq!(slot, RootSlot::Const { value: expected });
}
