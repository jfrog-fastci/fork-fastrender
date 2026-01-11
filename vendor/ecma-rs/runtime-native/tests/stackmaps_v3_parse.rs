#![cfg(all(target_arch = "x86_64", target_os = "linux"))]

use runtime_native::stackmaps::{Location, StackMap, StackMaps};

#[test]
fn parses_stackmaps_v3_fixture_and_builds_pc_index() {
  let bytes = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/stackmaps_v3.bin"
  ));

  let stackmap = StackMap::parse(bytes).expect("fixture stackmap should parse");
  assert_eq!(stackmap.version, 3);
  assert_eq!(stackmap.functions.len(), 1);
  assert_eq!(stackmap.constants, vec![1234605616436508552]);
  assert_eq!(stackmap.records.len(), 2);

  let func = &stackmap.functions[0];
  assert_eq!(func.address, 0);
  assert_eq!(func.stack_size, 24);
  assert_eq!(func.record_count, 2);

  let rec0 = &stackmap.records[0];
  assert_eq!(rec0.patchpoint_id, 99);
  assert_eq!(rec0.instruction_offset, 10);
  assert!(rec0.live_outs.is_empty());
  assert_eq!(rec0.locations.len(), 5);
  assert_eq!(
    rec0.locations[0],
    Location::Constant { size: 8, value: 123 }
  );
  assert_eq!(
    rec0.locations[1],
    Location::ConstIndex {
      size: 8,
      index: 0,
      value: 1234605616436508552
    }
  );
  assert_eq!(
    rec0.locations[2],
    Location::Register {
      size: 8,
      dwarf_reg: 5,
      offset: 0
    }
  );
  assert_eq!(
    rec0.locations[3],
    Location::Indirect {
      size: 8,
      dwarf_reg: 6,
      offset: 16
    }
  );
  assert_eq!(
    rec0.locations[4],
    Location::Direct {
      size: 8,
      dwarf_reg: 6,
      offset: -16
    }
  );

  let rec1 = &stackmap.records[1];
  assert_eq!(rec1.patchpoint_id, 100);
  assert_eq!(rec1.instruction_offset, 15);
  assert!(rec1.live_outs.is_empty());
  assert_eq!(
    rec1.locations[0],
    Location::Register {
      size: 8,
      dwarf_reg: 3,
      offset: 0
    }
  );

  let index = StackMaps::parse(bytes).expect("fixture stackmaps should parse + index");
  assert_eq!(index.lookup(10).unwrap().record.patchpoint_id, 99);
  assert_eq!(index.lookup(15).unwrap().record.patchpoint_id, 100);
  assert!(index.lookup(11).is_none());

  // Convenience API used by stack walkers returning `usize` PCs.
  assert_eq!(index.lookup_return_address(10).unwrap().record.patchpoint_id, 99);
}

#[test]
fn parses_patchpoint_live_outs() {
  // A minimal LLVM 18 patchpoint stackmap extracted from an object file. This exercises the
  // live-out header + entry parsing, which differs subtly from the location array.
  let bytes = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/patchpoint_liveouts.bin"
  ));

  let stackmap = StackMap::parse(bytes).expect("patchpoint stackmap should parse");
  assert_eq!(stackmap.records.len(), 1);
  let rec = &stackmap.records[0];
  assert!(rec.locations.is_empty());
  assert_eq!(rec.live_outs.len(), 1);
  assert_eq!(rec.live_outs[0].dwarf_reg, 7);
  assert_eq!(rec.live_outs[0].size, 8);

  let index = StackMaps::parse(bytes).expect("patchpoint stackmaps should parse + index");
  assert_eq!(index.lookup(4).unwrap().record.patchpoint_id, 1);
}

