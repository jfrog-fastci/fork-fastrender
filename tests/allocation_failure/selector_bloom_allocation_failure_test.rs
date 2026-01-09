use fastrender::dom::{
  build_selector_bloom_store, set_selector_bloom_enabled, set_selector_bloom_summary_bits, DomNode,
  DomNodeType,
};
use selectors::context::QuirksMode;
use std::collections::HashMap;
use std::mem;

use super::{fail_next_allocation, failed_allocs, lock_allocator};

#[test]
fn selector_bloom_store_allocation_failure_disables_bloom_without_aborting() {
  let _guard = lock_allocator();

  // Ensure environment-based configuration cannot disable the feature under test.
  set_selector_bloom_enabled(true);
  set_selector_bloom_summary_bits(1024);

  let root = DomNode {
    node_type: DomNodeType::Document {
      quirks_mode: QuirksMode::NoQuirks,
    },
    children: Vec::new(),
  };

  let map_len: usize = 10_000;
  let mut id_map: HashMap<*const DomNode, usize> = HashMap::with_capacity(map_len);
  for idx in 0..map_len {
    id_map.insert((idx + 1) as *const DomNode, idx + 1);
  }

  let alloc_size = map_len
    .saturating_add(1)
    .saturating_mul(mem::size_of::<[u64; 16]>());
  let alloc_align = mem::align_of::<[u64; 16]>();

  let start_failures = failed_allocs();
  fail_next_allocation(alloc_size, alloc_align);
  let store = build_selector_bloom_store(&root, &id_map);

  assert_eq!(
    failed_allocs(),
    start_failures + 1,
    "expected to trigger bloom store allocation failure"
  );
  assert!(
    store.is_none(),
    "expected bloom store to be skipped after allocation failure"
  );
}
