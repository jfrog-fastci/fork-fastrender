use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::style::display::Display;
use fastrender::style::float::Float;
use fastrender::style::values::Length;
use fastrender::BoxNode;
use fastrender::ComputedStyle;
use fastrender::FormattingContext;
use fastrender::FormattingContextType;
use std::sync::Arc;

/// Collapsible whitespace runs that are the *only* in-flow content in a block formatting context
/// must not generate empty line boxes.
///
/// An empty line box advances the block cursor and can push subsequent floats down, which is
/// particularly visible in float-based button groups (e.g. Bootstrap navbars).
#[test]
fn collapsible_whitespace_only_buffer_does_not_push_floats_down() {
  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;

  let whitespace_style = Arc::new(ComputedStyle::default());
  let whitespace_text = BoxNode::new_text(whitespace_style.clone(), "\n    ".to_string());
  let whitespace = BoxNode::new_anonymous_inline(whitespace_style.clone(), vec![whitespace_text]);

  let mut float_style = ComputedStyle::default();
  float_style.display = Display::InlineBlock;
  float_style.float = Float::Left;
  float_style.width = Some(Length::px(50.0));
  float_style.height = Some(Length::px(10.0));
  let float_style = Arc::new(float_style);

  let float_a = BoxNode::new_inline_block(float_style.clone(), FormattingContextType::Block, vec![]);
  let float_b = BoxNode::new_inline_block(float_style, FormattingContextType::Block, vec![]);

  let root = BoxNode::new_block(
    Arc::new(root_style),
    FormattingContextType::Block,
    vec![whitespace.clone(), float_a, whitespace.clone(), float_b, whitespace],
  );

  let bfc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(200.0, 200.0);
  let fragment = bfc.layout(&root, &constraints).expect("layout should succeed");

  let float_frags: Vec<_> = fragment
    .children
    .iter()
    .filter(|child| (child.bounds.width() - 50.0).abs() < 0.01 && (child.bounds.height() - 10.0).abs() < 0.01)
    .collect();

  assert_eq!(
    float_frags.len(),
    2,
    "expected two float fragments; got {} children",
    fragment.children.len()
  );
  assert!(
    float_frags[0].bounds.y().abs() < 0.01,
    "expected first float to start at y=0, got y={:.2}",
    float_frags[0].bounds.y()
  );
  assert!(
    float_frags[1].bounds.y().abs() < 0.01,
    "expected second float to start at y=0, got y={:.2}",
    float_frags[1].bounds.y()
  );
  assert!(
    (float_frags[1].bounds.x() - 50.0).abs() < 0.01,
    "expected second float to be placed next to the first, got x={:.2}",
    float_frags[1].bounds.x()
  );
}

