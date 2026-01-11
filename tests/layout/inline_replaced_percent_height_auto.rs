use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::inline::InlineFormattingContext;
use fastrender::style::display::Display;
use fastrender::style::values::Length;
use fastrender::tree::box_tree::BoxNode;
use fastrender::tree::box_tree::ReplacedType;
use fastrender::tree::fragment_tree::FragmentContent;
use fastrender::Size;
use std::sync::Arc;

fn find_replaced_height_by_box_id(root: &fastrender::FragmentNode, box_id: usize) -> Option<f32> {
  let mut stack = vec![root];
  while let Some(node) = stack.pop() {
    if matches!(
      node.content,
      FragmentContent::Replaced {
        box_id: Some(id), ..
      } if id == box_id
    ) {
      return Some(node.bounds.height());
    }
    for child in node.children.iter() {
      stack.push(child);
    }
  }
  None
}

#[test]
fn inline_replaced_percent_height_in_auto_height_container_computes_to_auto() {
  // CSS2.1 §10.5: Percentage `height` values compute to `auto` when the containing block's height
  // depends on content (i.e. it is not specified explicitly). Inline formatting context layout can
  // still run with a definite *available* height (e.g. the viewport); that must not be treated as
  // a valid percentage basis for in-flow content.

  let mut replaced_style = fastrender::ComputedStyle::default();
  replaced_style.display = Display::Inline;
  replaced_style.width = Some(Length::percent(100.0));
  replaced_style.height = Some(Length::percent(100.0));
  replaced_style.width_keyword = None;
  replaced_style.height_keyword = None;

  let mut replaced = BoxNode::new_replaced(
    Arc::new(replaced_style),
    ReplacedType::Canvas,
    Some(Size::new(100.0, 50.0)),
    None,
  );
  replaced.id = 1;

  let mut root_style = fastrender::ComputedStyle::default();
  root_style.display = Display::Inline;
  let mut root = BoxNode::new_inline(Arc::new(root_style), vec![replaced]);
  root.id = 2;

  // Definite available height (e.g. viewport), but no definite containing-block height for percent
  // resolution (block_percentage_base stays `None`).
  let constraints = LayoutConstraints::definite(200.0, 500.0);

  let fragment = InlineFormattingContext::new()
    .layout_with_floats(&root, &constraints, None, 0.0, 0.0)
    .expect("layout with floats");

  let height = find_replaced_height_by_box_id(&fragment, 1).expect("replaced fragment");
  let expected = 100.0; // width 200px with intrinsic ratio 2:1.
  assert!(
    (height - expected).abs() < 0.5,
    "expected `height:100%` to compute to `auto` and preserve intrinsic ratio ({}px), got {height}",
    expected
  );
}

