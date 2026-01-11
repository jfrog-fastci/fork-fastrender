use runtime_native::stackmaps::{parse_all_stackmaps, StackMaps};

fn build_stackmap_blob(function_address: u64, instruction_offset: u32, patchpoint_id: u64) -> Vec<u8> {
  let mut bytes = Vec::new();

  // StackMapHeader
  bytes.push(3); // version
  bytes.push(0); // reserved0
  bytes.extend_from_slice(&0u16.to_le_bytes()); // reserved1
  bytes.extend_from_slice(&1u32.to_le_bytes()); // num_functions
  bytes.extend_from_slice(&0u32.to_le_bytes()); // num_constants
  bytes.extend_from_slice(&1u32.to_le_bytes()); // num_records

  // FunctionRecord
  bytes.extend_from_slice(&function_address.to_le_bytes());
  bytes.extend_from_slice(&0u64.to_le_bytes()); // stack_size
  bytes.extend_from_slice(&1u64.to_le_bytes()); // record_count

  // CallSiteRecord
  bytes.extend_from_slice(&patchpoint_id.to_le_bytes());
  bytes.extend_from_slice(&instruction_offset.to_le_bytes());
  bytes.extend_from_slice(&0u16.to_le_bytes()); // reserved
  bytes.extend_from_slice(&1u16.to_le_bytes()); // num_locations

  // Location: register R#5, size 8
  bytes.push(1); // LocationKind::Register
  bytes.push(0); // reserved
  bytes.extend_from_slice(&8u16.to_le_bytes()); // size
  bytes.extend_from_slice(&5u16.to_le_bytes()); // dwarf_reg_num
  bytes.extend_from_slice(&0u16.to_le_bytes()); // reserved2
  bytes.extend_from_slice(&0i32.to_le_bytes()); // offset_or_small_constant

  // Align to 8 before live-outs.
  while bytes.len() % 8 != 0 {
    bytes.push(0);
  }

  // LiveOuts
  bytes.extend_from_slice(&0u16.to_le_bytes()); // num_live_outs
  bytes.extend_from_slice(&0u16.to_le_bytes()); // reserved

  // Align to 8 for next record/end.
  while bytes.len() % 8 != 0 {
    bytes.push(0);
  }

  bytes
}

#[test]
fn parse_all_supports_concatenated_blobs() {
  // Two blobs with the same patchpoint ID to ensure the merge does not assume
  // patchpoint IDs are globally unique across object files.
  let fixture_a = build_stackmap_blob(0x1000, 4, 1);
  let fixture_b = build_stackmap_blob(0x2000, 8, 1);

  // Simulate ELF linker concatenation of `.llvm_stackmaps` input sections.
  let mut fixture_multi = Vec::new();
  fixture_multi.extend_from_slice(&fixture_a);

  // Linkers align each input section to 8 bytes; simulate any required padding.
  let pad_len = (8 - (fixture_multi.len() % 8)) % 8;
  fixture_multi.extend(std::iter::repeat(0u8).take(pad_len));

  fixture_multi.extend_from_slice(&fixture_b);

  // Trailing padding at end-of-section should be ignored.
  fixture_multi.extend_from_slice(&[0u8; 8]);

  let stackmaps = parse_all_stackmaps(&fixture_multi).unwrap();
  assert_eq!(stackmaps.len(), 2);

  let registry = StackMaps::parse(&fixture_multi).unwrap();
  assert!(registry.lookup(0x1000 + 4).is_some());
  assert!(registry.lookup(0x2000 + 8).is_some());
}
