use fastrender::style::color::Rgba;
use fastrender::text::color_fonts::parse_cpal_palette;
use std::mem;
use super::{fail_next_allocation, failed_allocs, lock_allocator};

#[test]
fn cpal_palette_parse_survives_allocation_failure() {
  let _guard = lock_allocator();

  let num_entries: u16 = 12_345;
  let num_palettes: u16 = 1;
  let num_color_records: u16 = num_entries;

  // CPAL header:
  // version=0, numEntries, numPalettes, numColorRecords, colorOffset
  let color_offset: u32 = 14; // header (12) + palette index (2)
  let mut data = Vec::new();
  data.extend_from_slice(&0u16.to_be_bytes()); // version
  data.extend_from_slice(&num_entries.to_be_bytes());
  data.extend_from_slice(&num_palettes.to_be_bytes());
  data.extend_from_slice(&num_color_records.to_be_bytes());
  data.extend_from_slice(&color_offset.to_be_bytes());
  data.extend_from_slice(&0u16.to_be_bytes()); // palette index start

  // Pad to color offset and append BGRA records.
  data.resize(color_offset as usize, 0);
  data.resize(
    color_offset as usize + num_color_records as usize * 4,
    0xFF,
  );

  let parsed = parse_cpal_palette(&data, 0).expect("expected palette parse to succeed");
  assert_eq!(parsed.colors.len(), num_entries as usize);

  let alloc_size = num_entries as usize * mem::size_of::<Rgba>();
  let alloc_align = mem::align_of::<Rgba>();
  let start_failures = failed_allocs();
  fail_next_allocation(alloc_size, alloc_align);

  let parsed = parse_cpal_palette(&data, 0);
  assert_eq!(
    failed_allocs(),
    start_failures + 1,
    "expected to trigger palette allocation failure"
  );
  assert!(
    parsed.is_none(),
    "expected palette parsing to return None after allocation failure"
  );
}
