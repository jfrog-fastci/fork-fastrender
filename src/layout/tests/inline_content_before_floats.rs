use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::block::BlockFormattingContext;
use crate::style::display::Display;
use crate::style::float::Float;
use crate::style::types::{LineHeight, TextAlign};
use crate::style::values::Length;
use crate::tree::fragment_tree::FragmentContent;
use crate::tree::fragment_tree::FragmentNode;
use crate::BoxNode;
use crate::ComputedStyle;
use crate::FormattingContext;
use crate::FormattingContextType;
use crate::Point;
use crate::Rect;
use std::sync::Arc;

fn find_abs_bounds_by_box_id(fragment: &FragmentNode, box_id: usize) -> Option<Rect> {
  fn recurse(node: &FragmentNode, box_id: usize, origin: Point) -> Option<Rect> {
    let abs_bounds = node.bounds.translate(origin);
    let matches = match &node.content {
      FragmentContent::Block { box_id: Some(id) } => *id == box_id,
      FragmentContent::Inline {
        box_id: Some(id), ..
      } => *id == box_id,
      FragmentContent::Text {
        box_id: Some(id), ..
      } => *id == box_id,
      FragmentContent::Replaced {
        box_id: Some(id), ..
      } => *id == box_id,
      _ => false,
    };
    if matches {
      return Some(abs_bounds);
    }
    let child_origin = origin.translate(node.bounds.origin);
    for child in node.children.iter() {
      if let Some(found) = recurse(child, box_id, child_origin) {
        return Some(found);
      }
    }
    None
  }

  recurse(fragment, box_id, Point::ZERO)
}

/// A float that appears after inline-level content should align with the current line box top
/// (CSS 2.1 §9.5.1), rather than being pushed below the line boxes created by preceding inline
/// siblings.
#[test]
fn float_after_wrapped_inline_content_uses_current_line_top() {
  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;
  root_style.line_height = LineHeight::Length(Length::px(20.0));

  let mut inline_style = ComputedStyle::default();
  inline_style.display = Display::InlineBlock;
  inline_style.width = Some(Length::px(60.0));
  inline_style.height = Some(Length::px(10.0));
  let inline_style = Arc::new(inline_style);

  // Two 60px-wide inline blocks in a 100px container will wrap to two lines.
  let mut inline_a =
    BoxNode::new_inline_block(inline_style.clone(), FormattingContextType::Block, vec![]);
  inline_a.id = 1;
  let mut inline_b =
    BoxNode::new_inline_block(inline_style.clone(), FormattingContextType::Block, vec![]);
  inline_b.id = 2;

  let mut float_style = ComputedStyle::default();
  float_style.display = Display::InlineBlock;
  float_style.float = Float::Right;
  float_style.width = Some(Length::px(30.0));
  float_style.height = Some(Length::px(10.0));
  let mut float_box =
    BoxNode::new_inline_block(Arc::new(float_style), FormattingContextType::Block, vec![]);
  float_box.id = 3;

  let root = BoxNode::new_block(
    Arc::new(root_style),
    FormattingContextType::Block,
    vec![inline_a, inline_b, float_box],
  );

  let bfc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(100.0, 200.0);
  let fragment = bfc
    .layout(&root, &constraints)
    .expect("layout should succeed");

  let float_bounds = find_abs_bounds_by_box_id(&fragment, 3).expect("float fragment present");

  // The float comes after the wrapped inline content, so it should be placed on the second line.
  assert!(
    (float_bounds.y() - 20.0).abs() < 0.01,
    "expected float to start at y=20 (second line top), got y={:.2}",
    float_bounds.y()
  );
  assert!(
    (float_bounds.x() - 70.0).abs() < 0.01,
    "expected float-right to be placed at x=70, got x={:.2}",
    float_bounds.x()
  );
}

#[test]
fn float_after_inline_content_affects_text_align_center() {
  // Regression test: when a float follows inline content in a `text-align:center` block, the
  // current line box should shrink to the remaining width next to the float, and centering should
  // occur within that reduced line box (CSS 2.1 §9.5.1 + CSS Text `text-align`).
  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;
  root_style.text_align = TextAlign::Center;
  root_style.line_height = LineHeight::Length(Length::px(20.0));

  let mut inline_style = ComputedStyle::default();
  inline_style.display = Display::InlineBlock;
  inline_style.width = Some(Length::px(50.0));
  inline_style.height = Some(Length::px(10.0));
  let mut inline_box =
    BoxNode::new_inline_block(Arc::new(inline_style), FormattingContextType::Block, vec![]);
  inline_box.id = 1;

  let mut float_style = ComputedStyle::default();
  float_style.display = Display::InlineBlock;
  float_style.float = Float::Right;
  float_style.width = Some(Length::px(100.0));
  float_style.height = Some(Length::px(10.0));
  let mut float_box =
    BoxNode::new_inline_block(Arc::new(float_style), FormattingContextType::Block, vec![]);
  float_box.id = 2;

  let root = BoxNode::new_block(
    Arc::new(root_style),
    FormattingContextType::Block,
    vec![inline_box, float_box],
  );

  let bfc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(200.0, 200.0);
  let fragment = bfc
    .layout(&root, &constraints)
    .expect("layout should succeed");

  let inline_bounds = find_abs_bounds_by_box_id(&fragment, 1).expect("inline fragment present");
  let float_bounds = find_abs_bounds_by_box_id(&fragment, 2).expect("float fragment present");

  assert!(
    (float_bounds.x() - 100.0).abs() < 0.1,
    "expected float-right to be placed at x=100, got x={:.2}",
    float_bounds.x()
  );
  assert!(
    (inline_bounds.x() - 25.0).abs() < 0.1,
    "expected centered inline content to start at x=25 (centered within 100px remaining width), got x={:.2}",
    inline_bounds.x()
  );
  assert!(
    inline_bounds.max_x() <= float_bounds.x() + 0.01,
    "inline content should not overlap the float: inline_max_x={:.2} float_x={:.2}",
    inline_bounds.max_x(),
    float_bounds.x()
  );
}
