use stackmap::{StackMapLocation, StatepointRecordError, StatepointRecordView};

fn constant(value: u64) -> StackMapLocation {
  StackMapLocation::Constant { value, size: 8 }
}

fn reg(dwarf_reg: u16) -> StackMapLocation {
  StackMapLocation::Register {
    dwarf_reg,
    size: std::mem::size_of::<usize>() as u16,
  }
}

fn indirect(dwarf_reg: u16, offset: i32) -> StackMapLocation {
  StackMapLocation::Indirect {
    dwarf_reg,
    offset,
    size: std::mem::size_of::<usize>() as u16,
  }
}

#[test]
fn statepoint_record_view_error_too_few_locations() {
  let err = StatepointRecordView::new(&[]).unwrap_err();
  assert!(matches!(
    err,
    StatepointRecordError::TooFewLocations { locations_len: 0 }
  ));

  let locs = vec![constant(0), constant(0)];
  let err = StatepointRecordView::new(&locs).unwrap_err();
  assert!(matches!(
    err,
    StatepointRecordError::TooFewLocations { locations_len: 2 }
  ));
}

#[test]
fn statepoint_record_view_error_header_not_constant() {
  // Slot #1 (calling convention)
  let locs = vec![
    reg(1),
    constant(0),
    constant(0),
    // keep gc locations empty so we don't trip later validations
  ];
  let err = StatepointRecordView::new(&locs).unwrap_err();
  assert!(matches!(
    err,
    StatepointRecordError::HeaderNotConstant {
      header_index: 1,
      found_kind: "Register",
    }
  ));

  // Slot #2 (flags)
  let locs = vec![constant(0), indirect(7, 8), constant(0)];
  let err = StatepointRecordView::new(&locs).unwrap_err();
  assert!(matches!(
    err,
    StatepointRecordError::HeaderNotConstant {
      header_index: 2,
      found_kind: "Indirect",
    }
  ));

  // Slot #3 (deopt count)
  let locs = vec![
    constant(0),
    constant(0),
    StackMapLocation::ConstantIndex { index: 0, size: 8 },
  ];
  let err = StatepointRecordView::new(&locs).unwrap_err();
  assert!(matches!(
    err,
    StatepointRecordError::HeaderNotConstant {
      header_index: 3,
      found_kind: "ConstantIndex",
    }
  ));
}

#[test]
fn statepoint_record_view_error_deopt_count_exceeds_locations() {
  // Declares 1 deopt location but provides none.
  let locs = vec![constant(0), constant(0), constant(1)];
  let err = StatepointRecordView::new(&locs).unwrap_err();
  assert!(matches!(
    err,
    StatepointRecordError::DeoptCountExceedsLocations {
      deopt_count: 1,
      remaining_locations: 0,
    }
  ));
}

#[test]
#[cfg(target_pointer_width = "32")]
fn statepoint_record_view_error_deopt_count_too_large_for_usize() {
  // On 32-bit targets, deopt counts greater than `u32::MAX` cannot be represented in `usize`.
  let deopt_count = u64::from(u32::MAX) + 1;
  let locs = vec![constant(0), constant(0), constant(deopt_count)];
  let err = StatepointRecordView::new(&locs).unwrap_err();
  assert!(matches!(
    err,
    StatepointRecordError::DeoptCountTooLarge { deopt_count: v } if v == deopt_count
  ));
}

#[test]
fn statepoint_record_view_error_odd_gc_location_count() {
  // No deopt, but one trailing GC location (missing its pair).
  let locs = vec![constant(0), constant(0), constant(0), indirect(7, 8)];
  let err = StatepointRecordView::new(&locs).unwrap_err();
  assert!(matches!(
    err,
    StatepointRecordError::OddGcLocationCount { gc_locations_len: 1 }
  ));
}

