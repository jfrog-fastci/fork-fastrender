use fastrender::geometry::Size;
use fastrender::layout::constraints::{AvailableSpace, LayoutConstraints};
use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::{FormattingContext, IntrinsicSizingMode};
use fastrender::style::display::{Display, FormattingContextType};
use fastrender::style::position::Position;
use fastrender::style::types::{FlexDirection, JustifyContent};
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use fastrender::FontContext;
use std::sync::Arc;

#[test]
fn flex_intrinsic_block_size_ignores_percentage_width_when_base_indefinite() {
  // GitHub's hero uses a `width: 100%` flex container, but flex intrinsic sizing probes (min/max
  // content) run without a definite available inline size. Per CSS 2.1 §10.5, percentage widths
  // must behave as `auto` when the containing block width is indefinite; otherwise they collapse to
  // ~0px and text wraps into hundreds of lines, inflating the intrinsic block size.

  let viewport = Size::new(200.0, 200.0);
  let fc = FlexFormattingContext::with_viewport(viewport);

  let mut flex_style = ComputedStyle::default();
  flex_style.display = Display::Flex;
  flex_style.flex_direction = FlexDirection::Column;
  // Match github.com: percent widths are common in the hero subtree.
  flex_style.width = Some(Length::percent(100.0));

  let mut text_block_style = ComputedStyle::default();
  text_block_style.display = Display::Block;
  // The critical case: a descendant with `width:100%` whose percentage base is indefinite during an
  // intrinsic size probe. These must behave as `auto`, not collapse to 0px.
  text_block_style.width = Some(Length::percent(100.0));

  let words = std::iter::repeat("word").take(30).collect::<Vec<_>>().join(" ");
  let text = BoxNode::new_text(Arc::new(ComputedStyle::default()), words);
  let text_block =
    BoxNode::new_block(Arc::new(text_block_style), FormattingContextType::Block, vec![text]);

  let flex = BoxNode::new_block(
    Arc::new(flex_style),
    FormattingContextType::Flex,
    vec![text_block],
  );

  let max_content_height = fc
    .compute_intrinsic_block_size(&flex, IntrinsicSizingMode::MaxContent)
    .expect("intrinsic block size");

  assert!(
    max_content_height < 150.0,
    "expected max-content block size to stay small; got {:.1}px (percentage width likely collapsed)",
    max_content_height
  );
}

#[test]
fn fixed_flex_container_with_auto_height_does_not_stretch_to_viewport() {
  // Regression test for github.com: a `position: fixed` flex container with `height:auto` was being
  // laid out as if it had a definite available block-size (viewport height), which caused Taffy to
  // give it an oversized height and `justify-content:center` to vertically offset its children.

  let viewport = Size::new(200.0, 200.0);
  let fc = BlockFormattingContext::with_font_context_and_viewport(FontContext::new(), viewport);

  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;

  let mut fixed_style = ComputedStyle::default();
  fixed_style.display = Display::Flex;
  fixed_style.position = Position::Fixed;
  fixed_style.flex_direction = FlexDirection::Column;
  fixed_style.justify_content = JustifyContent::Center;
  fixed_style.width = Some(Length::percent(100.0));

  // Use wrapped inline content so the child's block-size depends on the available inline size.
  // If intrinsic sizing incorrectly collapses percent widths to ~0px, the text will wrap into many
  // lines and inflate the flex container's auto height.
  let mut text_block_style = ComputedStyle::default();
  text_block_style.display = Display::Block;

  let words = std::iter::repeat("word").take(30).collect::<Vec<_>>().join(" ");
  let text = BoxNode::new_text(Arc::new(ComputedStyle::default()), words);
  let text_block =
    BoxNode::new_block(Arc::new(text_block_style), FormattingContextType::Block, vec![text]);

  let fixed_flex = BoxNode::new_block(Arc::new(fixed_style), FormattingContextType::Flex, vec![text_block]);

  let root = BoxNode::new_block(Arc::new(root_style), FormattingContextType::Block, vec![fixed_flex]);

  let constraints = LayoutConstraints::new(
    AvailableSpace::Definite(viewport.width),
    AvailableSpace::Definite(viewport.height),
  );
  let fragment = fc.layout(&root, &constraints).expect("layout should succeed");

  let fixed_fragment = fragment
    .children
    .iter()
    .find(|child| matches!(child.style.as_ref().map(|s| s.position), Some(Position::Fixed)))
    .expect("fixed fragment present");

  let first_child = fixed_fragment.children.first().expect("flex child");
  assert!(
    (fixed_fragment.bounds.height() - first_child.bounds.height()).abs() < 0.5,
    "expected fixed flex container to shrink-wrap its only child (container_h={:.1}, child_h={:.1})",
    fixed_fragment.bounds.height(),
    first_child.bounds.height(),
  );
  assert!(
    first_child.bounds.y().abs() < 0.5,
    "expected first flex child y≈0px (got {:.1}px)",
    first_child.bounds.y()
  );
}
