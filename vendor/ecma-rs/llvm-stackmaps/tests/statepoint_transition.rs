use llvm_stackmaps::{Location, StackMaps, StatepointRecordView};

#[test]
fn statepoint_gc_transition_sets_flags_without_adding_locations() {
  let bytes = include_bytes!("fixtures/transition_bundle.stackmaps.bin");
  let maps = StackMaps::parse(bytes).expect("parse stackmaps fixture");
  let record = maps.records.first().expect("fixture should contain one record");

  let sp = StatepointRecordView::decode(record).expect("decode statepoint layout");
  assert_eq!(sp.call_conv, 0);
  assert_eq!(sp.flags, 1, "LLVM18 sets flags=1 for a gc-transition bundle");
  assert_eq!(sp.deopt_args.len(), 0);
  assert_eq!(sp.num_gc_roots(), 1);

  let mut pairs = sp.gc_root_pairs();
  let pair = pairs.next().unwrap();
  assert!(pairs.next().is_none());

  assert_eq!(
    (pair.base, pair.derived),
    (&record.locations[3], &record.locations[4]),
    "gc-transition should not insert extra locations before GC (base, derived) pairs"
  );
  assert!(matches!(pair.base, Location::Indirect { .. }));
  assert!(matches!(pair.derived, Location::Indirect { .. }));
}

