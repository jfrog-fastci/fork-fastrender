use llvm_stackmaps::{ParseOptions, StackMaps};

fn minimal_blob_with_records(num_records: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&[3, 0, 0, 0]); // version + reserved
    bytes.extend_from_slice(&(1u32).to_le_bytes()); // num_functions
    bytes.extend_from_slice(&(0u32).to_le_bytes()); // num_constants
    bytes.extend_from_slice(&num_records.to_le_bytes()); // num_records

    // Function entry: address=0, stack_size=0, record_count=num_records.
    bytes.extend_from_slice(&(0u64).to_le_bytes());
    bytes.extend_from_slice(&(0u64).to_le_bytes());
    bytes.extend_from_slice(&(num_records as u64).to_le_bytes());

    for i in 0..num_records {
        // Record header (16 bytes).
        bytes.extend_from_slice(&((i as u64) + 1).to_le_bytes()); // id
        bytes.extend_from_slice(&((i as u32) * 4).to_le_bytes()); // instruction offset
        bytes.extend_from_slice(&(0u16).to_le_bytes()); // reserved
        bytes.extend_from_slice(&(0u16).to_le_bytes()); // num locations

        // Live-out header (4 bytes): padding + num_liveouts=0.
        bytes.extend_from_slice(&(0u16).to_le_bytes());
        bytes.extend_from_slice(&(0u16).to_le_bytes());

        // Pad record end to 8 bytes: 16 + 4 = 20 => +4.
        bytes.extend_from_slice(&[0u8; 4]);
    }

    bytes
}

#[test]
fn parse_with_options_enforces_record_limit() {
    let bytes = minimal_blob_with_records(1);

    let mut opts = ParseOptions::DEFAULT;
    opts.max_records_per_blob = 0;

    let err = StackMaps::parse_with_options(&bytes, &opts).unwrap_err();
    assert!(
        err.message.contains("num_records") && err.message.contains("exceeds"),
        "unexpected error: {err}"
    );
}

#[test]
fn parse_with_options_enforces_location_limit_before_allocating() {
    // 1 record, but record header says num_locations=1. We only need to provide enough bytes for
    // the blob-level min-record-size check (24 bytes per record); the per-record location limit
    // check triggers before reading any location entries.
    let mut bytes = minimal_blob_with_records(1);
    let record_start = 16 + 24; // header + 1 function
    let num_locations_off = record_start + 8 + 4 + 2;
    bytes[num_locations_off..num_locations_off + 2].copy_from_slice(&(1u16).to_le_bytes());

    let mut opts = ParseOptions::DEFAULT;
    opts.max_locations_per_record = 0;

    let err = StackMaps::parse_with_options(&bytes, &opts).unwrap_err();
    assert!(
        err.message.contains("num_locations") && err.message.contains("exceeds"),
        "unexpected error: {err}"
    );
}

#[test]
fn parse_with_options_enforces_live_out_limit_before_allocating() {
    // 1 record, num_locations=0, but num_live_outs=1.
    let mut bytes = minimal_blob_with_records(1);
    let record_start = 16 + 24; // header + 1 function
    // For num_locations=0, live-out header starts at record_start + 16.
    let num_live_outs_off = record_start + 16 + 2;
    bytes[num_live_outs_off..num_live_outs_off + 2].copy_from_slice(&(1u16).to_le_bytes());

    let mut opts = ParseOptions::DEFAULT;
    opts.max_live_outs_per_record = 0;

    let err = StackMaps::parse_with_options(&bytes, &opts).unwrap_err();
    assert!(
        err.message.contains("num_live_outs") && err.message.contains("exceeds"),
        "unexpected error: {err}"
    );
}

#[test]
fn rejects_record_count_sum_overflow_in_function_table() {
    // Header: 2 functions, 0 records. Each function claims u64::MAX records, which overflows when
    // summing record_count across functions.
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&[3, 0, 0, 0]);
    bytes.extend_from_slice(&(2u32).to_le_bytes()); // num_functions
    bytes.extend_from_slice(&(0u32).to_le_bytes()); // num_constants
    bytes.extend_from_slice(&(0u32).to_le_bytes()); // num_records

    for _ in 0..2 {
        bytes.extend_from_slice(&(0u64).to_le_bytes()); // addr
        bytes.extend_from_slice(&(0u64).to_le_bytes()); // stack size
        bytes.extend_from_slice(&(u64::MAX).to_le_bytes()); // record_count
    }

    let err = StackMaps::parse(&bytes).unwrap_err();
    assert!(
        err.message.contains("overflow"),
        "unexpected error (wanted overflow rejection): {err}"
    );
}

