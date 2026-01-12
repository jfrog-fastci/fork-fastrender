use llvm_stackmaps::StackMaps;

#[test]
fn parse_rejects_unknown_location_kind() {
  let mut bytes = Vec::new();

  // Header
  bytes.extend_from_slice(&[3, 0, 0, 0]);
  bytes.extend_from_slice(&(1u32).to_le_bytes()); // num functions
  bytes.extend_from_slice(&(0u32).to_le_bytes()); // num constants
  bytes.extend_from_slice(&(1u32).to_le_bytes()); // num records

  // Function entry: 1 record.
  bytes.extend_from_slice(&(0u64).to_le_bytes()); // addr
  bytes.extend_from_slice(&(0u64).to_le_bytes()); // stack size
  bytes.extend_from_slice(&(1u64).to_le_bytes()); // record count

  // Record header with num_locations=1.
  bytes.extend_from_slice(&(1u64).to_le_bytes()); // id
  bytes.extend_from_slice(&(0u32).to_le_bytes()); // instruction offset
  bytes.extend_from_slice(&(0u16).to_le_bytes()); // reserved
  bytes.extend_from_slice(&(1u16).to_le_bytes()); // num locations

  // Location: unknown kind (99).
  bytes.push(99);
  bytes.push(0);
  bytes.extend_from_slice(&(8u16).to_le_bytes()); // size
  bytes.extend_from_slice(&(0u16).to_le_bytes()); // reg
  bytes.extend_from_slice(&(0u16).to_le_bytes()); // reserved1
  bytes.extend_from_slice(&(0i32).to_le_bytes()); // offset/val

  // Align to 8 before live-out header: record header 16 + loc 12 = 28 => +4.
  bytes.extend_from_slice(&[0u8; 4]);

  // Live-out header: padding + num_liveouts=0.
  bytes.extend_from_slice(&(0u16).to_le_bytes());
  bytes.extend_from_slice(&(0u16).to_le_bytes());

  // Align record end: 32 + 4 = 36 => +4.
  bytes.extend_from_slice(&[0u8; 4]);

  let err = StackMaps::parse(&bytes).unwrap_err();
  assert!(
    err.message.contains("unknown location kind"),
    "unexpected error: {err}"
  );
}

#[test]
fn parse_rejects_negative_constantindex() {
  let mut bytes = Vec::new();

  // Header
  bytes.extend_from_slice(&[3, 0, 0, 0]);
  bytes.extend_from_slice(&(1u32).to_le_bytes()); // num functions
  bytes.extend_from_slice(&(1u32).to_le_bytes()); // num constants
  bytes.extend_from_slice(&(1u32).to_le_bytes()); // num records

  // Function entry: 1 record.
  bytes.extend_from_slice(&(0u64).to_le_bytes()); // addr
  bytes.extend_from_slice(&(0u64).to_le_bytes()); // stack size
  bytes.extend_from_slice(&(1u64).to_le_bytes()); // record count

  // One constant (unused; ConstantIndex will error before lookup).
  bytes.extend_from_slice(&(0u64).to_le_bytes());

  // Record header with num_locations=1.
  bytes.extend_from_slice(&(1u64).to_le_bytes()); // id
  bytes.extend_from_slice(&(0u32).to_le_bytes()); // instruction offset
  bytes.extend_from_slice(&(0u16).to_le_bytes()); // reserved
  bytes.extend_from_slice(&(1u16).to_le_bytes()); // num locations

  // Location: ConstantIndex with a negative index.
  bytes.push(5);
  bytes.push(0);
  bytes.extend_from_slice(&(8u16).to_le_bytes()); // size
  bytes.extend_from_slice(&(0u16).to_le_bytes()); // reg
  bytes.extend_from_slice(&(0u16).to_le_bytes()); // reserved1
  bytes.extend_from_slice(&(-1i32).to_le_bytes()); // index

  // Align to 8 before live-out header: record header 16 + loc 12 = 28 => +4.
  bytes.extend_from_slice(&[0u8; 4]);

  // Live-out header: padding + num_liveouts=0.
  bytes.extend_from_slice(&(0u16).to_le_bytes());
  bytes.extend_from_slice(&(0u16).to_le_bytes());

  // Align record end: 32 + 4 = 36 => +4.
  bytes.extend_from_slice(&[0u8; 4]);

  let err = StackMaps::parse(&bytes).unwrap_err();
  assert!(
    err.message.contains("ConstantIndex is negative"),
    "unexpected error: {err}"
  );
}

#[test]
fn parse_rejects_constantindex_out_of_bounds() {
  let mut bytes = Vec::new();

  // Header
  bytes.extend_from_slice(&[3, 0, 0, 0]);
  bytes.extend_from_slice(&(1u32).to_le_bytes()); // num functions
  bytes.extend_from_slice(&(1u32).to_le_bytes()); // num constants
  bytes.extend_from_slice(&(1u32).to_le_bytes()); // num records

  // Function entry: 1 record.
  bytes.extend_from_slice(&(0u64).to_le_bytes()); // addr
  bytes.extend_from_slice(&(0u64).to_le_bytes()); // stack size
  bytes.extend_from_slice(&(1u64).to_le_bytes()); // record count

  // One constant at index 0.
  bytes.extend_from_slice(&(0x1122_3344_5566_7788u64).to_le_bytes());

  // Record header with num_locations=1.
  bytes.extend_from_slice(&(1u64).to_le_bytes()); // id
  bytes.extend_from_slice(&(0u32).to_le_bytes()); // instruction offset
  bytes.extend_from_slice(&(0u16).to_le_bytes()); // reserved
  bytes.extend_from_slice(&(1u16).to_le_bytes()); // num locations

  // Location: ConstantIndex(1) (out of bounds for a 1-entry constants table).
  bytes.push(5);
  bytes.push(0);
  bytes.extend_from_slice(&(8u16).to_le_bytes()); // size
  bytes.extend_from_slice(&(0u16).to_le_bytes()); // reg
  bytes.extend_from_slice(&(0u16).to_le_bytes()); // reserved1
  bytes.extend_from_slice(&(1i32).to_le_bytes());

  // Align to 8 before live-out header: record header 16 + loc 12 = 28 => +4.
  bytes.extend_from_slice(&[0u8; 4]);

  // Live-out header: padding + num_liveouts=0.
  bytes.extend_from_slice(&(0u16).to_le_bytes());
  bytes.extend_from_slice(&(0u16).to_le_bytes());

  // Align record end: 32 + 4 = 36 => +4.
  bytes.extend_from_slice(&[0u8; 4]);

  let err = StackMaps::parse(&bytes).unwrap_err();
  assert!(
    err.message.contains("ConstantIndex 1 out of bounds"),
    "unexpected error: {err}"
  );
}

#[test]
fn parse_rejects_callsite_pc_overflow() {
  let mut bytes = Vec::new();

  // Header
  bytes.extend_from_slice(&[3, 0, 0, 0]);
  bytes.extend_from_slice(&(1u32).to_le_bytes()); // num functions
  bytes.extend_from_slice(&(0u32).to_le_bytes()); // num constants
  bytes.extend_from_slice(&(1u32).to_le_bytes()); // num records

  // Function entry: 1 record, but address + instruction_offset overflows u64.
  bytes.extend_from_slice(&(u64::MAX - 5).to_le_bytes()); // addr
  bytes.extend_from_slice(&(0u64).to_le_bytes()); // stack size
  bytes.extend_from_slice(&(1u64).to_le_bytes()); // record count

  // Record header with num_locations=0.
  bytes.extend_from_slice(&(1u64).to_le_bytes()); // id
  bytes.extend_from_slice(&(10u32).to_le_bytes()); // instruction offset
  bytes.extend_from_slice(&(0u16).to_le_bytes()); // reserved
  bytes.extend_from_slice(&(0u16).to_le_bytes()); // num locations

  // Live-out header (already aligned): padding + num_liveouts=0.
  bytes.extend_from_slice(&(0u16).to_le_bytes());
  bytes.extend_from_slice(&(0u16).to_le_bytes());

  // Align record end: 16 + 4 = 20 => +4.
  bytes.extend_from_slice(&[0u8; 4]);

  let err = StackMaps::parse(&bytes).unwrap_err();
  assert!(
    err.message.contains("callsite_pc overflow"),
    "unexpected error: {err}"
  );
}
