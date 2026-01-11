#![cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]

use runtime_native::stackmaps::{StackMap, StackMapError};
use runtime_native::test_util::TestRuntimeGuard;

#[test]
fn malformed_stackmaps_do_not_panic_or_attempt_huge_allocations() {
  let _rt = TestRuntimeGuard::new();
  // Each case is intentionally too short for the declared counts, so the parser should return
  // `UnexpectedEof` *without* panicking (e.g. from `Vec::with_capacity`).
  let cases: &[&[u8]] = &[
    // Header with NumFunctions = u32::MAX but no function records.
    &[
      3, 0, 0, 0, // version + reserved
      0xff, 0xff, 0xff, 0xff, // NumFunctions
      0, 0, 0, 0, // NumConstants
      0, 0, 0, 0, // NumRecords
    ],
    // Header with NumRecords = u32::MAX but no records.
    &[
      3, 0, 0, 0, // version + reserved
      0, 0, 0, 0, // NumFunctions
      0, 0, 0, 0, // NumConstants
      0xff, 0xff, 0xff, 0xff, // NumRecords
    ],
    // One record with NumLocations = u16::MAX but no location entries.
    &[
      3, 0, 0, 0, // version + reserved
      0, 0, 0, 0, // NumFunctions
      0, 0, 0, 0, // NumConstants
      1, 0, 0, 0, // NumRecords
      // Record header (16 bytes)
      0, 0, 0, 0, 0, 0, 0, 0, // patchpoint_id
      0, 0, 0, 0, // instruction_offset
      0, 0, // reserved
      0xff, 0xff, // num_locations
    ],
    // One record with NumLiveOuts = u16::MAX but no live-out entries.
    &[
      3, 0, 0, 0, // version + reserved
      0, 0, 0, 0, // NumFunctions
      0, 0, 0, 0, // NumConstants
      1, 0, 0, 0, // NumRecords
      // Record header (16 bytes)
      0, 0, 0, 0, 0, 0, 0, 0, // patchpoint_id
      0, 0, 0, 0, // instruction_offset
      0, 0, // reserved
      0, 0, // num_locations
      // Live-out header (aligned to 8 already at this point)
      0, 0, // padding
      0xff, 0xff, // num_live_outs
    ],
  ];

  for bytes in cases {
    let res = std::panic::catch_unwind(|| StackMap::parse(bytes));
    assert!(res.is_ok(), "StackMap::parse panicked for bytes={bytes:?}");
    match res.unwrap() {
      Err(StackMapError::UnexpectedEof) => {}
      other => panic!("expected UnexpectedEof, got {other:?} for bytes={bytes:?}"),
    }
  }
}
