#![cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]

use runtime_native::stackmaps::{Location, StackMap, StackMaps, StackSize};

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
  assert_eq!(func.stack_size, StackSize::Known(24));
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

#[test]
fn parses_unaligned_live_out_header_between_records() {
  // Synthetic StackMap v3 blob with *two* records. The first record ends its locations array on a
  // 4-byte boundary (1 location => 16 + 12 = 28 bytes) and then emits the live-out header
  // immediately, without the usual 8-byte alignment padding.
  //
  // This shape has been observed from some toolchains in the wild and would cause an "aligned-only"
  // parser to desynchronize when the next record follows.
  let mut bytes = Vec::new();
  bytes.push(3); // version
  bytes.push(0); // reserved0
  bytes.extend_from_slice(&0u16.to_le_bytes()); // reserved1
  bytes.extend_from_slice(&1u32.to_le_bytes()); // num_functions
  bytes.extend_from_slice(&0u32.to_le_bytes()); // num_constants
  bytes.extend_from_slice(&2u32.to_le_bytes()); // num_records

  // One function record covering both records.
  bytes.extend_from_slice(&0u64.to_le_bytes()); // function_address
  bytes.extend_from_slice(&0u64.to_le_bytes()); // stack_size
  bytes.extend_from_slice(&2u64.to_le_bytes()); // record_count

  // Record 1: 1 location, unaligned live-out header with num_live_outs=0.
  bytes.extend_from_slice(&1u64.to_le_bytes()); // patchpoint_id
  bytes.extend_from_slice(&16u32.to_le_bytes()); // instruction_offset (pc=16)
  bytes.extend_from_slice(&0u16.to_le_bytes()); // reserved
  bytes.extend_from_slice(&1u16.to_le_bytes()); // num_locations
  // Location[0] = Constant 7
  bytes.extend_from_slice(&[4, 0]); // Constant, reserved
  bytes.extend_from_slice(&8u16.to_le_bytes()); // size
  bytes.extend_from_slice(&0u16.to_le_bytes()); // dwarf_reg
  bytes.extend_from_slice(&0u16.to_le_bytes()); // reserved
  bytes.extend_from_slice(&7i32.to_le_bytes()); // small const
  // Live-out header immediately after the location (no 8-byte alignment padding).
  bytes.extend_from_slice(&0u16.to_le_bytes()); // padding
  bytes.extend_from_slice(&0u16.to_le_bytes()); // num_live_outs

  // Record 2: 0 locations, normal live-out header, ends aligned.
  bytes.extend_from_slice(&2u64.to_le_bytes()); // patchpoint_id
  bytes.extend_from_slice(&32u32.to_le_bytes()); // instruction_offset (pc=32)
  bytes.extend_from_slice(&0u16.to_le_bytes()); // reserved
  bytes.extend_from_slice(&0u16.to_le_bytes()); // num_locations
  // Live-out header (padding + num_live_outs=0).
  bytes.extend_from_slice(&0u16.to_le_bytes());
  bytes.extend_from_slice(&0u16.to_le_bytes());
  while bytes.len() % 8 != 0 {
    bytes.push(0);
  }

  let stackmaps = StackMaps::parse(&bytes).expect("parse synthetic stackmaps");
  let pcs: Vec<(u64, u64)> = stackmaps
    .iter()
    .map(|(pc, callsite)| (pc, callsite.record.patchpoint_id))
    .collect();
  assert_eq!(pcs, vec![(16, 1), (32, 2)]);
}

#[test]
fn parses_unaligned_live_out_header_with_entries_between_records() {
  // Like `parses_unaligned_live_out_header_between_records`, but record0 also contains a live-out
  // entry. This exercises the "aligned parse hits UnexpectedEof" fallback path (since attempting to
  // interpret the first live-out entry as the aligned header yields an absurd `num_live_outs`).
  let mut bytes = Vec::new();
  bytes.push(3); // version
  bytes.push(0); // reserved0
  bytes.extend_from_slice(&0u16.to_le_bytes()); // reserved1
  bytes.extend_from_slice(&1u32.to_le_bytes()); // num_functions
  bytes.extend_from_slice(&0u32.to_le_bytes()); // num_constants
  bytes.extend_from_slice(&2u32.to_le_bytes()); // num_records

  // One function record covering both records.
  bytes.extend_from_slice(&0u64.to_le_bytes()); // function_address
  bytes.extend_from_slice(&0u64.to_le_bytes()); // stack_size
  bytes.extend_from_slice(&2u64.to_le_bytes()); // record_count

  // Record 1: 1 location, unaligned live-out header with num_live_outs=1.
  bytes.extend_from_slice(&1u64.to_le_bytes()); // patchpoint_id
  bytes.extend_from_slice(&16u32.to_le_bytes()); // instruction_offset (pc=16)
  bytes.extend_from_slice(&0u16.to_le_bytes()); // reserved
  bytes.extend_from_slice(&1u16.to_le_bytes()); // num_locations
  // Location[0] = Constant 7
  bytes.extend_from_slice(&[4, 0]); // Constant, reserved
  bytes.extend_from_slice(&8u16.to_le_bytes()); // size
  bytes.extend_from_slice(&0u16.to_le_bytes()); // dwarf_reg
  bytes.extend_from_slice(&0u16.to_le_bytes()); // reserved
  bytes.extend_from_slice(&7i32.to_le_bytes()); // small const
  // Live-out header immediately after the location (no 8-byte alignment padding).
  bytes.extend_from_slice(&0u16.to_le_bytes()); // padding
  bytes.extend_from_slice(&1u16.to_le_bytes()); // num_live_outs
  // LiveOut[0]: dwarf_reg=7, reserved=0, size=8.
  bytes.extend_from_slice(&7u16.to_le_bytes());
  bytes.push(0);
  bytes.push(8);
  // Pad to 8-byte record alignment.
  while bytes.len() % 8 != 0 {
    bytes.push(0);
  }

  // Record 2: 0 locations, normal live-out header, ends aligned.
  bytes.extend_from_slice(&2u64.to_le_bytes()); // patchpoint_id
  bytes.extend_from_slice(&32u32.to_le_bytes()); // instruction_offset (pc=32)
  bytes.extend_from_slice(&0u16.to_le_bytes()); // reserved
  bytes.extend_from_slice(&0u16.to_le_bytes()); // num_locations
  bytes.extend_from_slice(&0u16.to_le_bytes()); // padding
  bytes.extend_from_slice(&0u16.to_le_bytes()); // num_live_outs
  while bytes.len() % 8 != 0 {
    bytes.push(0);
  }

  let stackmaps = StackMaps::parse(&bytes).expect("parse synthetic stackmaps");
  assert_eq!(stackmaps.raw().records[0].live_outs.len(), 1);
  assert_eq!(stackmaps.raw().records[0].live_outs[0].dwarf_reg, 7);
  assert_eq!(stackmaps.raw().records[0].live_outs[0].size, 8);

  let pcs: Vec<(u64, u64)> = stackmaps
    .iter()
    .map(|(pc, callsite)| (pc, callsite.record.patchpoint_id))
    .collect();
  assert_eq!(pcs, vec![(16, 1), (32, 2)]);
}

#[test]
fn pc_index_supports_multiple_records_with_duplicate_patchpoint_id() {
  let bytes = include_bytes!("fixtures/bin/stackmap_dup_id_two_records_x86_64.bin");

  let stackmap = StackMap::parse(bytes).expect("fixture stackmap should parse");
  assert_eq!(stackmap.version, 3);
  assert_eq!(stackmap.functions.len(), 1);
  assert_eq!(stackmap.records.len(), 2);

  let func = &stackmap.functions[0];
  assert_eq!(func.record_count, 2);

  // LLVM does not guarantee `patchpoint_id` uniqueness. The PC index must still contain
  // one entry per record/callsite.
  assert_eq!(stackmap.records[0].patchpoint_id, stackmap.records[1].patchpoint_id);

  let index = StackMaps::parse(bytes).expect("fixture stackmaps should parse + index");
  assert_eq!(index.callsites().len(), 2);

  for rec in &stackmap.records {
    let pc = func.address + rec.instruction_offset as u64;
    let callsite = index.lookup(pc).expect("missing callsite for record pc");
    assert_eq!(callsite.record.patchpoint_id, rec.patchpoint_id);
    assert_eq!(callsite.record.instruction_offset, rec.instruction_offset);
  }
}

#[test]
fn pc_index_uses_record_count_to_associate_records_to_functions() {
  let bytes = include_bytes!("fixtures/bin/stackmap_dup_id_two_funcs_x86_64.bin");

  let stackmap = StackMap::parse(bytes).expect("fixture stackmap should parse");
  assert_eq!(stackmap.version, 3);
  assert_eq!(stackmap.functions.len(), 2);
  assert_eq!(stackmap.records.len(), 2);
  assert_eq!(stackmap.functions[0].record_count, 1);
  assert_eq!(stackmap.functions[1].record_count, 1);

  // Ensure the fixture actually has two distinct functions; this is what lets the test detect
  // record-to-function misassociation bugs.
  assert_ne!(stackmap.functions[0].stack_size, stackmap.functions[1].stack_size);

  // Both records intentionally share a patchpoint id to ensure parsers don't treat it as unique.
  assert_eq!(stackmap.records[0].patchpoint_id, stackmap.records[1].patchpoint_id);

  let index = StackMaps::parse(bytes).expect("fixture stackmaps should parse + index");
  assert_eq!(index.callsites().len(), 2);

  // Records are associated to functions purely by each `FunctionRecord.RecordCount` in order.
  let expected = [
    (&stackmap.functions[0], &stackmap.records[0]),
    (&stackmap.functions[1], &stackmap.records[1]),
  ];
  for (func, rec) in expected {
    let pc = func.address + rec.instruction_offset as u64;
    let callsite = index.lookup(pc).expect("missing callsite for record pc");
    assert_eq!(callsite.stack_size, func.stack_size);
    assert_eq!(callsite.record.patchpoint_id, rec.patchpoint_id);
    assert_eq!(callsite.record.instruction_offset, rec.instruction_offset);
  }
}
