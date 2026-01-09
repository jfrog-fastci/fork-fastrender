use fastrender::text::otvar::item_variation_store::{
  parse_delta_set_index_map, DeltaSetIndex, ParseError,
};
use std::mem;
use super::{fail_next_allocation, failed_allocs, lock_allocator};

#[test]
fn delta_set_index_map_allocation_failure_is_reported_instead_of_aborting() {
  let _guard = lock_allocator();

  let map_count: u32 = 1_000_000;
  let entry_format: u16 = 0x0000; // entry_size=1, inner_index_bit_count=1
  let entry_size = 1usize;

  let mut data = vec![0u8; 8 + map_count as usize * entry_size];
  data[0..2].copy_from_slice(&1u16.to_be_bytes()); // format 1 => map_count is u32
  data[2..4].copy_from_slice(&entry_format.to_be_bytes());
  data[4..8].copy_from_slice(&map_count.to_be_bytes());

  let alloc_size = map_count as usize * mem::size_of::<DeltaSetIndex>();
  let alloc_align = mem::align_of::<DeltaSetIndex>();
  let start_failures = failed_allocs();
  fail_next_allocation(alloc_size, alloc_align);

  let result = parse_delta_set_index_map(&data);
  assert_eq!(
    failed_allocs(),
    start_failures + 1,
    "expected to trigger allocation failure"
  );
  assert_eq!(
    result,
    Err(ParseError::InvalidValue(
      "delta set index map allocation failed"
    ))
  );
}
