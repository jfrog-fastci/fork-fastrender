use fastrender::css::types::StyleSheet;
use fastrender::dom::{DomNode, DomNodeType};
use fastrender::style::cascade::apply_style_set_with_media_target_and_imports_cached_with_deadline;
use fastrender::style::media::MediaContext;
use fastrender::style::style_set::StyleSet;
use std::mem;
use super::{fail_next_allocation, failed_allocs, lock_allocator};

fn build_large_dom(node_count: usize) -> DomNode {
  // Build a shallow DOM (depth 2) to avoid deep recursion in cascade on debug builds.
  let child_count = node_count.saturating_sub(1);
  let mut children = Vec::with_capacity(child_count);
  for _ in 0..child_count {
    children.push(DomNode {
      node_type: DomNodeType::Element {
        tag_name: "div".to_string(),
        namespace: String::new(),
        attributes: Vec::new(),
      },
      children: Vec::new(),
    });
  }
  DomNode {
    node_type: DomNodeType::Document {
      quirks_mode: selectors::context::QuirksMode::NoQuirks,
      scripting_enabled: true,
    },
    children,
  }
}

#[test]
fn cascade_survives_dom_maps_allocation_failure() {
  let _guard = lock_allocator();

  // 1 root + 12_344 children -> 12_345 total nodes.
  let node_count = 12_345usize;
  let dom = build_large_dom(node_count);
  let style_set = StyleSet::from_document(StyleSheet::new());
  let media_ctx = MediaContext::screen(800.0, 600.0);

  // Fail the `tree_scope_prefixes: Vec<u32>` allocation inside `DomMaps::new`.
  let alloc_len = node_count + 1;
  let alloc_size = alloc_len * mem::size_of::<u32>();
  let alloc_align = mem::align_of::<u32>();
  let start_failures = failed_allocs();
  fail_next_allocation(alloc_size, alloc_align);

  let result = apply_style_set_with_media_target_and_imports_cached_with_deadline(
    &dom,
    &style_set,
    &media_ctx,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
  );

  assert_eq!(
    failed_allocs(),
    start_failures + 1,
    "expected to trigger DomMaps allocation failure"
  );
  assert!(
    result.is_err(),
    "expected cascade to return an error (not abort) after allocation failure"
  );
}
