use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::style::display::Display;
use fastrender::style::float::Float;
use fastrender::style::values::Length;
use fastrender::BoxNode;
use fastrender::ComputedStyle;
use fastrender::FragmentContent;
use fastrender::FormattingContext;
use fastrender::FormattingContextType;
use fastrender::Size;
use fastrender::tree::box_tree::ReplacedType;
use std::sync::Arc;

#[test]
fn inline_level_float_does_not_expand_non_bfc_block_height() {
  // Regression test for a common HTML pattern (e.g. Phoronix article list):
  //
  //   <div class="anon-wrapper">
  //     <a><img style="float:left"></a>
  //   </div>
  //   <div class="details">…</div>
  //
  // The anonymous block wrapper does *not* establish a new BFC, so its auto height must ignore the
  // float (classic "clearfix" behavior). The following block should start at the same y and have
  // its line boxes shortened by the float.

  let mut img_style = ComputedStyle::default();
  img_style.display = Display::Inline;
  img_style.float = Float::Left;
  img_style.width = Some(Length::px(100.0));
  img_style.height = Some(Length::px(50.0));
  let img = BoxNode::new_replaced(
    Arc::new(img_style),
    ReplacedType::Canvas,
    Some(Size::new(100.0, 50.0)),
    Some(100.0 / 50.0),
  );

  let anchor = BoxNode::new_inline(Arc::new(ComputedStyle::default()), vec![img]);

  // Simulate the anonymous block wrapper generated when block containers contain inline runs.
  let wrapper = BoxNode::new_anonymous_block(Arc::new(ComputedStyle::default()), vec![anchor]);

  let details_text = BoxNode::new_text(Arc::new(ComputedStyle::default()), "Hello world".into());
  let details = BoxNode::new_block(
    Arc::new(ComputedStyle::default()),
    FormattingContextType::Block,
    vec![details_text],
  );

  let root = BoxNode::new_block(
    Arc::new(ComputedStyle::default()),
    FormattingContextType::Block,
    vec![wrapper, details],
  );

  let bfc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(200.0, 200.0);
  let fragment = bfc.layout(&root, &constraints).expect("layout should succeed");

  assert_eq!(
    fragment.children.len(),
    2,
    "expected wrapper + details fragments; got {} children",
    fragment.children.len()
  );
  let wrapper_frag = &fragment.children[0];
  let details_frag = &fragment.children[1];

  assert!(
    wrapper_frag.bounds.height().abs() < 0.01,
    "expected non-BFC wrapper height to ignore float (0px); got {:.2}",
    wrapper_frag.bounds.height()
  );
  assert!(
    details_frag.bounds.y().abs() < 0.01,
    "expected details block to start at y=0 (not below float); got y={:.2}",
    details_frag.bounds.y()
  );

  // The float should still affect line box placement for the following block.
  fn first_line_x(fragment: &fastrender::FragmentNode, acc_x: f32) -> Option<f32> {
    if matches!(fragment.content, FragmentContent::Line { .. }) {
      return Some(acc_x + fragment.bounds.x());
    }
    for child in fragment.children.iter() {
      if let Some(x) = first_line_x(child, acc_x + fragment.bounds.x()) {
        return Some(x);
      }
    }
    None
  }
  let line_x = first_line_x(details_frag, 0.0).expect("expected a line box in details fragment");
  assert!(
    (line_x - 100.0).abs() < 0.5,
    "expected details line boxes to be shifted right by float width (x≈100); got x={:.2}",
    line_x
  );
}
