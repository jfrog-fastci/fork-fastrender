use std::mem::size_of;

use llvm_stackmaps::StackMaps;

// Regression test for hostile inputs that previously could trigger very large allocations while
// still being "structurally valid" enough to pass the parser's EOF checks.
#[test]
fn rejects_record_count_exceeding_allocation_budget() {
    let max_alloc_bytes = llvm_stackmaps::ParseOptions::DEFAULT.max_alloc_bytes;

    let per_record = size_of::<llvm_stackmaps::StackMapRecord>() + size_of::<llvm_stackmaps::Callsite>();
    let num_records: usize = (max_alloc_bytes / per_record) + 1;
    let num_records_u32: u32 = num_records.try_into().expect("num_records must fit u32");

    // Minimal StackMap v3 blob:
    // - header (16)
    // - 1 function (24)
    // - records: `num_records` records with 0 locations and 0 live-outs (24 bytes each)
    let total_len = 16usize + 24usize + (num_records * 24usize);
    let mut bytes = vec![0u8; total_len];

    // Header
    bytes[0] = 3;
    bytes[4..8].copy_from_slice(&(1u32).to_le_bytes()); // num_functions
    bytes[8..12].copy_from_slice(&(0u32).to_le_bytes()); // num_constants
    bytes[12..16].copy_from_slice(&num_records_u32.to_le_bytes()); // num_records

    // Function entry: address=0, stack_size=0, record_count=num_records.
    bytes[32..40].copy_from_slice(&(num_records as u64).to_le_bytes());

    let err = StackMaps::parse(&bytes).unwrap_err();
    assert!(
        err.message.contains("parser budget"),
        "unexpected error (wanted budget rejection): {err}"
    );
}
