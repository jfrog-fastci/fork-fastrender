use llvm_stackmaps::{Location, StackMaps, StatepointRecordView};

#[test]
fn statepoint_skips_deopt_bundle_locations() {
  let bytes = include_bytes!("fixtures/deopt_bundle2.stackmaps.bin");
  let maps = StackMaps::parse(bytes).unwrap();

  let record = maps.records.first().expect("fixture should contain one record");

  let sp = StatepointRecordView::decode(record).expect("record should decode as a statepoint");
  assert_eq!(sp.flags, 0);
  assert_eq!(sp.deopt_args.len(), 2);

  // The deopt operands themselves are not GC roots.
  assert_eq!(sp.deopt_args[0].as_u64(), Some(1));
  assert_eq!(sp.deopt_args[1].as_u64(), Some(2));

  // After skipping the deopt args, the remaining locations are (base, derived) pairs.
  assert_eq!(sp.num_gc_roots(), 1);
  let mut pairs = sp.gc_root_pairs();
  let pair = pairs.next().unwrap();
  assert!(pairs.next().is_none());

  assert_eq!(
    (pair.base, pair.derived),
    (&record.locations[5], &record.locations[6]),
    "GC roots must start after the (callconv, flags, NumDeoptArgs) header and deopt args"
  );
  assert!(matches!(pair.base, Location::Indirect { .. }));
  assert!(matches!(pair.derived, Location::Indirect { .. }));
}

#[test]
fn statepoint_skips_indirect_deopt_locations() {
  let bytes = include_bytes!("fixtures/deopt_var.stackmaps.bin");
  let maps = StackMaps::parse(bytes).unwrap();

  let record = maps.records.first().expect("fixture should contain one record");

  let sp = StatepointRecordView::decode(record).expect("record should decode as a statepoint");
  assert_eq!(sp.deopt_args.len(), 1);
  assert!(matches!(sp.deopt_args[0], Location::Indirect { .. }));

  assert_eq!(sp.num_gc_roots(), 1);
  let mut pairs = sp.gc_root_pairs();
  let pair = pairs.next().unwrap();
  assert!(pairs.next().is_none());
  assert_eq!((pair.base, pair.derived), (&record.locations[4], &record.locations[5]));
}
