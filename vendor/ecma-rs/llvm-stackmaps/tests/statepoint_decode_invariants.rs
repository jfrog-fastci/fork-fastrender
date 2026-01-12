use llvm_stackmaps::{Location, StackMapRecord, StackMaps, StatepointRecordView};

use proptest::prelude::*;

fn record_with_locations(locations: Vec<Location>) -> StackMapRecord {
  StackMapRecord {
    id: 0,
    instruction_offset: 0,
    callsite_pc: 0,
    locations,
    live_outs: vec![],
  }
}

#[test]
fn statepoint_decode_rejects_too_few_locations() {
  let record = record_with_locations(vec![]);
  assert!(StatepointRecordView::decode(&record).is_none());

  let record = record_with_locations(vec![Location::Constant { size: 8, value: 0 }]);
  assert!(StatepointRecordView::decode(&record).is_none());
}

#[test]
fn statepoint_decode_rejects_non_constant_header_locations() {
  // Header #0 must be a constant.
  let record = record_with_locations(vec![
    Location::Register {
      size: 8,
      dwarf_reg: 1,
    },
    Location::Constant { size: 8, value: 0 },
    Location::Constant { size: 8, value: 0 },
  ]);
  assert!(StatepointRecordView::decode(&record).is_none());

  // Header #2 (deopt count) must be a constant and must fit in usize.
  let record = record_with_locations(vec![
    Location::Constant { size: 8, value: 0 },
    Location::Constant { size: 8, value: 0 },
    Location::Constant { size: 8, value: -1 },
  ]);
  assert!(StatepointRecordView::decode(&record).is_none());
}

#[test]
fn statepoint_decode_rejects_deopt_count_add_overflow() {
  // `deopt_end = 3 + num_deopt` must not overflow usize.
  //
  // Use ConstantIndex so we can represent `usize::MAX` even on 64-bit (where it exceeds i64::MAX).
  let record = record_with_locations(vec![
    Location::Constant { size: 8, value: 0 },
    Location::Constant { size: 8, value: 0 },
    Location::ConstantIndex {
      size: 8,
      index: 0,
      value: usize::MAX as u64,
    },
  ]);
  assert!(StatepointRecordView::decode(&record).is_none());
}

#[test]
fn statepoint_decode_rejects_deopt_count_exceeding_locations() {
  // Header says there are 2 deopt args, but we only provide 1.
  let record = record_with_locations(vec![
    Location::Constant { size: 8, value: 0 },
    Location::Constant { size: 8, value: 0 },
    Location::Constant { size: 8, value: 2 },
    Location::Constant {
      size: 8,
      value: 123,
    },
  ]);
  assert!(StatepointRecordView::decode(&record).is_none());
}

#[test]
fn statepoint_decode_rejects_odd_gc_root_locations() {
  // Header is valid with 0 deopt args, but GC roots must be in (base, derived) pairs.
  let record = record_with_locations(vec![
    Location::Constant { size: 8, value: 0 },
    Location::Constant { size: 8, value: 0 },
    Location::Constant { size: 8, value: 0 },
    Location::Register {
      size: 8,
      dwarf_reg: 1,
    },
  ]);
  assert!(StatepointRecordView::decode(&record).is_none());
}

#[test]
fn statepoint_decode_supports_multiple_location_kinds_and_multiple_gc_roots() {
  let record = record_with_locations(vec![
    Location::Constant { size: 8, value: 8 },
    Location::Constant { size: 8, value: 1 },
    Location::Constant { size: 8, value: 0 }, // deopt args
    // Root #0: Register → Indirect
    Location::Register {
      size: 8,
      dwarf_reg: 1,
    },
    Location::Indirect {
      size: 8,
      dwarf_reg: 7,
      offset: 8,
    },
    // Root #1: Direct → Register
    Location::Direct {
      size: 8,
      dwarf_reg: 7,
      offset: 16,
    },
    Location::Register {
      size: 8,
      dwarf_reg: 2,
    },
  ]);

  let sp = StatepointRecordView::decode(&record).expect("decode statepoint");
  assert_eq!(sp.call_conv, 8);
  assert_eq!(sp.flags, 1);
  assert_eq!(sp.deopt_args.len(), 0);
  assert_eq!(sp.num_gc_roots(), 2);

  let pairs = sp.gc_root_pairs().collect::<Vec<_>>();
  assert_eq!(pairs.len(), 2);
  assert_eq!(pairs[0].base, &record.locations[3]);
  assert_eq!(pairs[0].derived, &record.locations[4]);
  assert_eq!(pairs[1].base, &record.locations[5]);
  assert_eq!(pairs[1].derived, &record.locations[6]);
}

fn root_location() -> impl Strategy<Value = Location> {
  prop_oneof![
    (0u16..256).prop_map(|dwarf_reg| Location::Register { size: 8, dwarf_reg }),
    (0u16..256, any::<i32>()).prop_map(|(dwarf_reg, offset)| Location::Direct {
      size: 8,
      dwarf_reg,
      offset
    }),
    (0u16..256, any::<i32>()).prop_map(|(dwarf_reg, offset)| Location::Indirect {
      size: 8,
      dwarf_reg,
      offset
    }),
  ]
}

fn deopt_location() -> impl Strategy<Value = Location> {
  prop_oneof![
    root_location(),
    any::<i32>().prop_map(|value| Location::Constant {
      size: 8,
      value: i64::from(value)
    }),
  ]
}

fn valid_statepoint_record() -> impl Strategy<Value = (StackMapRecord, u64, u64, usize, usize)> {
  (0u64..64, 0u64..64, 0usize..8, 0usize..8).prop_flat_map(
    |(call_conv, flags, deopt_count, root_count)| {
      let gc_flat_len = root_count.checked_mul(2).expect("root_count * 2 overflow");
      let deopt_args = prop::collection::vec(deopt_location(), deopt_count);
      let gc_roots_flat = prop::collection::vec(root_location(), gc_flat_len);

      (
        Just(call_conv),
        Just(flags),
        Just(deopt_count),
        Just(root_count),
        deopt_args,
        gc_roots_flat,
      )
        .prop_map(
          |(call_conv, flags, deopt_count, root_count, deopt_args, gc_roots_flat)| {
            let mut locations = Vec::with_capacity(3 + deopt_args.len() + gc_roots_flat.len());
            locations.push(Location::Constant {
              size: 8,
              value: call_conv as i64,
            });
            locations.push(Location::Constant {
              size: 8,
              value: flags as i64,
            });
            locations.push(Location::Constant {
              size: 8,
              value: deopt_count as i64,
            });
            locations.extend(deopt_args);
            locations.extend(gc_roots_flat);

            (
              record_with_locations(locations),
              call_conv,
              flags,
              deopt_count,
              root_count,
            )
          },
        )
    },
  )
}

proptest! {
    #[test]
    fn statepoint_decode_roundtrips_valid_layout(case in valid_statepoint_record()) {
        let (record, call_conv, flags, deopt_count, root_count) = case;
        let sp = StatepointRecordView::decode(&record).expect("valid statepoint layout must decode");

        prop_assert_eq!(sp.call_conv, call_conv);
        prop_assert_eq!(sp.flags, flags);
        prop_assert_eq!(sp.deopt_args.len(), deopt_count);
        prop_assert_eq!(sp.num_gc_roots(), root_count);
        prop_assert_eq!(sp.gc_root_pairs().count(), root_count);
    }
}

#[test]
fn llvm18_fixture_two_statepoints_decodes_two_statepoints() {
  let bytes = include_bytes!("fixtures/llvm18_stackmaps/two_statepoints.stackmaps.bin");
  let maps = StackMaps::parse(bytes).unwrap();
  assert_eq!(maps.records.len(), 2);

  let sp0 =
    StatepointRecordView::decode(&maps.records[0]).expect("record #0 should be a statepoint");
  let sp1 =
    StatepointRecordView::decode(&maps.records[1]).expect("record #1 should be a statepoint");

  assert_eq!(sp0.call_conv, sp1.call_conv);
  assert_eq!(sp0.flags, sp1.flags);
  assert_eq!(sp0.deopt_args.len(), sp1.deopt_args.len());
  assert_eq!(sp0.num_gc_roots(), sp1.num_gc_roots());
}
