#![cfg(all(target_arch = "x86_64", target_os = "linux"))]

use runtime_native::stackmaps::{Location, StackMap, StackMaps};

#[test]
fn parses_stackmaps_v3_fixture_and_builds_pc_index() {
  let bytes = include_bytes!("fixtures/bin/stackmaps_v3.bin");

  let stackmap = StackMap::parse(bytes).expect("fixture stackmap should parse");
  assert_eq!(stackmap.version, 3);
  assert_eq!(stackmap.functions.len(), 1);
  assert_eq!(stackmap.constants, vec![1234605616436508552]);
  assert_eq!(stackmap.records.len(), 2);

  let func = &stackmap.functions[0];
  assert_eq!(func.address, 0);
  assert_eq!(func.stack_size, 24);
  assert_eq!(func.record_count, 2);

  let rec99 = stackmap
    .records
    .iter()
    .find(|r| r.patchpoint_id == 99)
    .expect("missing patchpoint_id=99");
  assert!(rec99.live_outs.is_empty());
  assert_eq!(rec99.locations.len(), 5);
  assert_eq!(
    rec99.locations[0],
    Location::Constant { size: 8, value: 123 }
  );
  assert_eq!(
    rec99.locations[1],
    Location::ConstIndex {
      size: 8,
      index: 0,
      value: 1234605616436508552
    }
  );
  // The remaining 3 locations are target/codegen dependent (register allocation, stack layout),
  // but must not be additional statepoint header constants.
  assert!(
    rec99.locations[2..]
      .iter()
      .all(|l| !matches!(l, Location::Constant { .. } | Location::ConstIndex { .. })),
    "expected non-header locations after the constant prefix: {:?}",
    &rec99.locations[2..]
  );

  let rec100 = stackmap
    .records
    .iter()
    .find(|r| r.patchpoint_id == 100)
    .expect("missing patchpoint_id=100");
  assert!(rec100.live_outs.is_empty());
  assert_eq!(rec100.locations.len(), 1);

  let index = StackMaps::parse(bytes).expect("fixture stackmaps should parse + index");
  let pcs: Vec<(u64, u64)> = index
    .iter()
    .map(|(pc, callsite)| (pc, callsite.record.patchpoint_id))
    .collect();
  assert_eq!(pcs.len(), 2);

  // Ensure lookup works for every indexed callsite PC.
  for (pc, patchpoint_id) in &pcs {
    assert_eq!(index.lookup(*pc).unwrap().record.patchpoint_id, *patchpoint_id);
    assert_eq!(
      index.lookup_return_address(*pc as usize).unwrap().record.patchpoint_id,
      *patchpoint_id
    );
  }

  let missing_pc = pcs.iter().map(|(pc, _)| *pc).max().unwrap().wrapping_add(1);
  assert!(index.lookup(missing_pc).is_none());
}

#[test]
fn parses_patchpoint_live_outs() {
  // A minimal LLVM 18 patchpoint stackmap extracted from an object file. This exercises the
  // live-out header + entry parsing, which differs subtly from the location array.
  let bytes = include_bytes!("fixtures/bin/patchpoint_liveouts.bin");

  let stackmap = StackMap::parse(bytes).expect("patchpoint stackmap should parse");
  assert_eq!(stackmap.records.len(), 1);
  let rec = &stackmap.records[0];
  assert!(rec.locations.is_empty());
  assert_eq!(rec.live_outs.len(), 1);
  assert_eq!(rec.live_outs[0].dwarf_reg, 7);
  assert_eq!(rec.live_outs[0].size, 8);

  let index = StackMaps::parse(bytes).expect("patchpoint stackmaps should parse + index");
  let (pc, callsite) = index.iter().next().expect("expected 1 callsite");
  assert_eq!(callsite.record.patchpoint_id, 1);
  assert_eq!(index.lookup(pc).unwrap().record.patchpoint_id, 1);
  assert!(index.lookup(pc.wrapping_add(1)).is_none());
}
