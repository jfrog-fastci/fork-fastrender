use fastrender::css::parser::parse_stylesheet;
use fastrender::dom::{DomNode, DomNodeType};
use fastrender::style::cascade::apply_style_set_with_media_target_and_imports_cached_with_deadline;
use fastrender::style::media::MediaContext;
use fastrender::style::style_set::StyleSet;
use super::{
  fail_next_allocation, failed_allocs, lock_allocator, recorded_allocation_layout,
  reset_recorded_allocation_layout,
};

fn build_stylesheet_with_rules(rule_count: usize) -> fastrender::css::types::StyleSheet {
  let mut css = String::new();
  for idx in 0..rule_count {
    css.push_str(&format!(".c{idx} {{ color: rgb(1, 2, 3); }}\n"));
  }
  parse_stylesheet(&css).expect("stylesheet should parse")
}

fn rule_sets_content_allocation_layout(rule_count: usize) -> (usize, usize) {
  // `Vec<bool>` is bitpacked and its internal allocation strategy is an implementation detail.
  // Capture the actual allocation layout by running the same `try_reserve_exact` call used by
  // `RuleIndex::new`.
  reset_recorded_allocation_layout();
  let mut probe: Vec<bool> = Vec::new();
  probe
    .try_reserve_exact(rule_count)
    .expect("probe allocation should succeed");
  recorded_allocation_layout().expect("expected probe to perform an allocation")
}

#[test]
fn cascade_survives_rule_index_allocation_failure() {
  let _guard = lock_allocator();

  let rule_count = 20_003usize;
  let stylesheet = build_stylesheet_with_rules(rule_count);
  let style_set = StyleSet::from_document(stylesheet);

  let dom = DomNode {
    node_type: DomNodeType::Document {
      quirks_mode: selectors::context::QuirksMode::NoQuirks,
    },
    children: vec![DomNode {
      node_type: DomNodeType::Element {
        tag_name: "div".to_string(),
        namespace: String::new(),
        attributes: Vec::new(),
      },
      children: Vec::new(),
    }],
  };

  let media_ctx = MediaContext::screen(800.0, 600.0);

  // Fail the `rule_sets_content: Vec<bool>` allocation inside `RuleIndex::new`.
  let (alloc_size, alloc_align) = rule_sets_content_allocation_layout(rule_count);
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
    "expected to trigger rule index allocation failure"
  );
  assert!(
    result.is_err(),
    "expected cascade to return an error (not abort) after allocation failure"
  );
}
