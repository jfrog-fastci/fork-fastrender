use fastrender::css::types::StyleSheet;
use fastrender::dom::{DomNode, DomNodeType};
use fastrender::style::cascade::apply_style_set_with_media_target_and_imports_cached_with_deadline;
use fastrender::style::media::MediaContext;
use fastrender::style::style_set::StyleSet;
use std::mem;
use super::{fail_next_allocation, failed_allocs, lock_allocator};

fn build_dom_with_many_classes(class_count: usize) -> DomNode {
  let mut class_value = String::new();
  for idx in 0..class_count {
    if idx != 0 {
      class_value.push(' ');
    }
    class_value.push('a');
  }

  DomNode {
    node_type: DomNodeType::Document {
      quirks_mode: selectors::context::QuirksMode::NoQuirks,
      scripting_enabled: true,
    },
    children: vec![DomNode {
      node_type: DomNodeType::Element {
        tag_name: "div".to_string(),
        namespace: String::new(),
        attributes: vec![("class".to_string(), class_value)],
      },
      children: Vec::new(),
    }],
  }
}

#[test]
fn cascade_survives_selector_key_cache_allocation_failure() {
  let _guard = lock_allocator();

  let class_count = 20_003usize;
  let dom = build_dom_with_many_classes(class_count);
  let style_set = StyleSet::from_document(StyleSheet::new());
  let media_ctx = MediaContext::screen(800.0, 600.0);

  // Fail the `DomSelectorKeyCache::class_keys: Vec<u64>` allocation inside `DomMaps::new`.
  let alloc_size = class_count * mem::size_of::<u64>();
  let alloc_align = mem::align_of::<u64>();
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
    "expected to trigger selector key cache allocation failure"
  );
  assert!(
    result.is_err(),
    "expected cascade to return an error (not abort) after allocation failure"
  );
}
