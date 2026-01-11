use fastrender::layout::constraints::AvailableSpace;
use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::types::FlexDirection;
use fastrender::style::types::IntrinsicSizeKeyword;
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::{BoxNode, ReplacedType};
use fastrender::{FragmentContent, FragmentNode, Size};
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
fn percent_height_in_auto_height_block_container_computes_to_auto() {
  // CSS2.1 §10.5: Percentage `height` values compute to `auto` when the containing block's height
  // is not specified explicitly. Block layout frequently runs with a definite available height
  // (e.g. the viewport), but that is not a valid basis for resolving `height:100%` when the
  // containing block itself is `height:auto`.

  let mut fixed_child_style = ComputedStyle::default();
  fixed_child_style.display = Display::Block;
  fixed_child_style.height = Some(Length::px(10.0));
  fixed_child_style.height_keyword = None;
  let fixed_child =
    BoxNode::new_block(Arc::new(fixed_child_style), FormattingContextType::Block, vec![]);

  let mut flex_style = ComputedStyle::default();
  flex_style.display = Display::Flex;
  flex_style.flex_direction = FlexDirection::Column;
  flex_style.height = Some(Length::percent(100.0));
  flex_style.height_keyword = None;
  let flex_box = BoxNode::new_block(Arc::new(flex_style), FormattingContextType::Flex, vec![fixed_child]);

  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Block;
  let parent = BoxNode::new_block(Arc::new(parent_style), FormattingContextType::Block, vec![flex_box]);

  let fc = BlockFormattingContext::new();
  let fragment = fc
    .layout(
      &parent,
      &LayoutConstraints::new(
        AvailableSpace::Definite(100.0),
        AvailableSpace::Definite(100.0),
      ),
    )
    .expect("layout should succeed");

  let flex_fragment = fragment.children.first().expect("flex fragment");
  assert!(
    (flex_fragment.bounds.height() - 10.0).abs() < 0.5,
    "expected `height:100%` to compute to `auto` when containing block height is auto (got {})",
    flex_fragment.bounds.height()
  );
}

#[test]
fn percent_height_in_min_content_height_block_container_computes_to_auto() {
  // CSS2.1 §10.5: Percentage `height` values compute to `auto` when the containing block's height
  // depends on content. This includes intrinsic sizing keywords like `height: min-content`, which
  // still require laying out descendants to determine the used block size.
  //
  // Regression (hbr.org): A `height:100%` inline replaced element inside a `height:min-content`
  // container incorrectly resolved against a finite percentage base, stretching the image to a
  // tall column instead of using its intrinsic aspect ratio.

  let mut img_style = ComputedStyle::default();
  img_style.display = Display::Inline;
  img_style.width = Some(Length::percent(100.0));
  img_style.height = Some(Length::percent(100.0));
  let mut img = BoxNode::new_replaced(
    Arc::new(img_style),
    ReplacedType::Canvas,
    Some(Size::new(300.0, 150.0)),
    None,
  );
  img.id = 1;

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Block;
  container_style.height_keyword = Some(IntrinsicSizeKeyword::MinContent);
  let mut container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Block,
    vec![img],
  );
  container.id = 2;

  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;
  let root = BoxNode::new_block(
    Arc::new(root_style),
    FormattingContextType::Block,
    vec![container],
  );

  let fc = BlockFormattingContext::new();
  let fragment = fc
    .layout(
      &root,
      &LayoutConstraints::new(
        AvailableSpace::Definite(200.0),
        AvailableSpace::Definite(200.0),
      ),
    )
    .expect("layout should succeed");

  let img_fragment = find_fragment_by_box_id(&fragment, 1).expect("image fragment");
  let container_fragment = find_fragment_by_box_id(&fragment, 2).expect("container fragment");
  assert!(
    (img_fragment.bounds.height() - 100.0).abs() < 0.5,
    "expected `height:100%` to compute to `auto` and preserve intrinsic ratio (got {})",
    img_fragment.bounds.height()
  );
  assert!(
    container_fragment.bounds.height() < 180.0,
    "`height:min-content` must not inflate intrinsic block sizing probes (got container height {})",
    container_fragment.bounds.height()
  );
}
