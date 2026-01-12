use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::block::BlockFormattingContext;
use crate::layout::formatting_context::FormattingContext;
use crate::style::display::Display;
use crate::style::display::FormattingContextType;
use crate::style::position::Position;
use crate::style::values::Length;
use crate::style::ComputedStyle;
use crate::tree::box_tree::BoxNode;
use std::sync::Arc;

/// Collapsible whitespace that appears immediately before an out-of-flow positioned descendant
/// (`position:absolute`/`fixed`) must not force an extra empty line box.
///
/// Berkeley's skyline search input places the submit button out-of-flow and relies on the
/// containing block sizing to match the in-flow input control. If whitespace around the abspos
/// button creates an extra (empty) line box, the container's height inflates and the button's
/// `height: calc(100% - 8px)` becomes too large.
#[test]
fn collapsible_whitespace_before_abspos_does_not_create_empty_line_box() {
  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;

  let mut fill_style = ComputedStyle::default();
  fill_style.display = Display::InlineBlock;
  fill_style.width = Some(Length::px(200.0));
  fill_style.height = Some(Length::px(42.0));
  let fill_style = Arc::new(fill_style);

  let whitespace_style = Arc::new(ComputedStyle::default());
  let whitespace_text = BoxNode::new_text(whitespace_style.clone(), "\n    ".to_string());
  let whitespace = BoxNode::new_anonymous_inline(whitespace_style, vec![whitespace_text]);

  let mut abs_style = ComputedStyle::default();
  abs_style.display = Display::InlineBlock;
  abs_style.position = Position::Absolute;
  abs_style.width = Some(Length::px(10.0));
  abs_style.height = Some(Length::px(10.0));
  let abs_style = Arc::new(abs_style);

  let root_without_abspos = BoxNode::new_block(
    Arc::new(root_style.clone()),
    FormattingContextType::Block,
    vec![BoxNode::new_inline_block(
      fill_style.clone(),
      FormattingContextType::Block,
      vec![],
    )],
  );

  let root = BoxNode::new_block(
    Arc::new(root_style),
    FormattingContextType::Block,
    vec![
      BoxNode::new_inline_block(fill_style, FormattingContextType::Block, vec![]),
      whitespace,
      BoxNode::new_inline_block(abs_style, FormattingContextType::Block, vec![]),
    ],
  );

  let bfc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(200.0, 200.0);
  let expected = bfc
    .layout(&root_without_abspos, &constraints)
    .expect("layout without abspos should succeed")
    .bounds
    .height();
  let fragment = bfc
    .layout(&root, &constraints)
    .expect("layout should succeed");

  assert!(
    (fragment.bounds.height() - expected).abs() < 0.01,
    "expected block height to match the in-flow-only case (got {:.2}, expected {:.2})",
    fragment.bounds.height(),
    expected
  );
}
