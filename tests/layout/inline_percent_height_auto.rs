use fastrender::layout::constraints::{AvailableSpace, LayoutConstraints};
use fastrender::layout::contexts::inline::InlineFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::{Display, FormattingContextType};
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use fastrender::{FragmentContent, FragmentNode};
use std::sync::Arc;

fn find_fragment_by_box_id<'a>(fragment: &'a FragmentNode, box_id: usize) -> Option<&'a FragmentNode> {
  let matches = match &fragment.content {
    FragmentContent::Block { box_id: Some(id) } => *id == box_id,
    FragmentContent::Inline { box_id: Some(id), .. } => *id == box_id,
    FragmentContent::Replaced { box_id: Some(id), .. } => *id == box_id,
    FragmentContent::Text { box_id: Some(id), .. } => *id == box_id,
    _ => false,
  };
  if matches {
    return Some(fragment);
  }
  for child in fragment.children.iter() {
    if let Some(found) = find_fragment_by_box_id(child, box_id) {
      return Some(found);
    }
  }
  None
}

#[test]
fn percent_height_in_inline_formatting_context_computes_to_auto_when_containing_block_height_is_auto()
{
  // Regression test motivated by `dropbox.com` CTA buttons: an inline formatting context can have a
  // definite available height (e.g. viewport), but percentage heights on inline-level formatting
  // contexts (`display:inline-grid`) must not resolve unless the containing block's height is
  // definite (CSS2.1 §10.5).

  let mut fixed_child_style = ComputedStyle::default();
  fixed_child_style.display = Display::Block;
  fixed_child_style.height = Some(Length::px(10.0));
  fixed_child_style.height_keyword = None;
  let fixed_child =
    BoxNode::new_block(Arc::new(fixed_child_style), FormattingContextType::Block, vec![]);

  let mut grid_style = ComputedStyle::default();
  grid_style.display = Display::InlineGrid;
  grid_style.height = Some(Length::percent(100.0));
  grid_style.height_keyword = None;
  let mut grid = BoxNode::new_inline_block(
    Arc::new(grid_style),
    FormattingContextType::Grid,
    vec![fixed_child],
  );
  grid.id = 1;

  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Inline;
  let root = BoxNode::new_inline(Arc::new(root_style), vec![grid]);

  let constraints = LayoutConstraints::new(
    AvailableSpace::Definite(100.0),
    // The inline formatting context can be laid out with a definite available height (e.g. the
    // viewport), but that must not be treated as a percentage basis for `height: 100%` when the
    // containing block is `height: auto`.
    AvailableSpace::Definite(100.0),
  );
  let ifc = InlineFormattingContext::new();
  let fragment = ifc.layout(&root, &constraints).expect("layout should succeed");

  let grid_fragment = find_fragment_by_box_id(&fragment, 1).expect("grid fragment");
  assert!(
    (grid_fragment.bounds.height() - 10.0).abs() < 0.5,
    "expected `height:100%` to compute to `auto` (got {})",
    grid_fragment.bounds.height()
  );
}

