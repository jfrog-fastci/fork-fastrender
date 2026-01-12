use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::types::IntrinsicSizeKeyword;
use fastrender::style::values::Length;
use fastrender::tree::box_tree::BoxNode;
use fastrender::tree::box_tree::ReplacedType;
use fastrender::Size;
use std::sync::Arc;

#[test]
fn height_min_content_on_block_container_behaves_like_auto() {
  // `height: min-content` is often used in the wild to cancel out a fixed height and return to a
  // content-based height. Ensure it behaves like `auto` for block containers whose block size is
  // determined by their in-flow content.

  let mut replaced_style = fastrender::ComputedStyle::default();
  replaced_style.display = Display::Inline;
  replaced_style.width = Some(Length::percent(100.0));
  replaced_style.height = Some(Length::percent(100.0));
  replaced_style.width_keyword = None;
  replaced_style.height_keyword = None;

  let mut replaced_auto = BoxNode::new_replaced(
    Arc::new(replaced_style.clone()),
    ReplacedType::Canvas,
    Some(Size::new(100.0, 50.0)),
    None,
  );
  replaced_auto.id = 1;

  let mut replaced_min = BoxNode::new_replaced(
    Arc::new(replaced_style),
    ReplacedType::Canvas,
    Some(Size::new(100.0, 50.0)),
    None,
  );
  replaced_min.id = 2;

  let mut auto_style = fastrender::ComputedStyle::default();
  auto_style.display = Display::Block;
  let auto_container = BoxNode::new_block(
    Arc::new(auto_style),
    FormattingContextType::Block,
    vec![replaced_auto],
  );

  let mut min_style = fastrender::ComputedStyle::default();
  min_style.display = Display::Block;
  min_style.height = None;
  min_style.height_keyword = Some(IntrinsicSizeKeyword::MinContent);
  let min_container = BoxNode::new_block(
    Arc::new(min_style),
    FormattingContextType::Block,
    vec![replaced_min],
  );

  let mut root_style = fastrender::ComputedStyle::default();
  root_style.display = Display::Block;
  let root = BoxNode::new_block(
    Arc::new(root_style),
    FormattingContextType::Block,
    vec![auto_container, min_container],
  );

  let fc = BlockFormattingContext::new();
  let fragment = fc
    .layout(&root, &LayoutConstraints::definite_width(200.0))
    .expect("layout");

  let auto_fragment = &fragment.children[0];
  let min_fragment = &fragment.children[1];
  assert!(
    (auto_fragment.bounds.height() - min_fragment.bounds.height()).abs() <= 0.5,
    "expected `height:min-content` to behave like `auto` (auto={} min-content={})",
    auto_fragment.bounds.height(),
    min_fragment.bounds.height()
  );
}
