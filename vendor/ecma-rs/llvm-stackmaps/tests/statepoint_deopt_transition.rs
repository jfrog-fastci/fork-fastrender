use llvm_stackmaps::{Location, StackMaps, StatepointRecordView};

#[test]
fn statepoint_with_deopt_and_gc_transition_skips_deopt_and_sets_flags() {
  let bytes = include_bytes!("fixtures/deopt_transition.stackmaps.bin");
  let maps = StackMaps::parse(bytes).expect("parse stackmaps fixture");
  let record = maps.records.first().expect("fixture should contain one record");

  let sp = StatepointRecordView::decode(record).expect("decode statepoint layout");
  assert_eq!(sp.call_conv, 0);
  assert_eq!(sp.flags, 1);
  assert_eq!(sp.deopt_args.len(), 2);
  assert_eq!(sp.deopt_args[0].as_u64(), Some(1));
  assert_eq!(sp.deopt_args[1].as_u64(), Some(2));
  assert_eq!(sp.num_gc_roots(), 1);

  let mut pairs = sp.gc_root_pairs();
  let pair = pairs.next().unwrap();
  assert!(pairs.next().is_none());

  assert_eq!(
    (pair.base, pair.derived),
    (&record.locations[5], &record.locations[6]),
    "gc-transition should not change the statepoint layout: GC pairs start after header + deopt"
  );
  assert!(matches!(pair.base, Location::Indirect { .. }));
  assert!(matches!(pair.derived, Location::Indirect { .. }));
}

