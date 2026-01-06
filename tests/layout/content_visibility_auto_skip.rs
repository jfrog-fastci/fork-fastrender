use std::sync::Arc;

use fastrender::layout::constraints::{AvailableSpace, LayoutConstraints};
use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::style::display::{Display, FormattingContextType};
use fastrender::style::types::{ContainIntrinsicSizeAxis, ContentVisibility};
use fastrender::style::values::Length;
use fastrender::text::font_loader::FontContext;
use fastrender::tree::box_tree::BoxNode;
use fastrender::{ComputedStyle, FormattingContext, Size};

fn block_with_style(style: ComputedStyle, children: Vec<BoxNode>) -> BoxNode {
  BoxNode::new_block(Arc::new(style), FormattingContextType::Block, children)
}

#[test]
fn content_visibility_auto_does_not_skip_unknown_block_size() {
  let viewport = Size::new(200.0, 200.0);
  let fc = BlockFormattingContext::with_font_context_and_viewport(FontContext::new(), viewport);

  let mut spacer_style = ComputedStyle::default();
  spacer_style.display = Display::Block;
  spacer_style.height = Some(Length::px(240.0));
  let spacer = block_with_style(spacer_style, vec![]);

  let mut tall_child_style = ComputedStyle::default();
  tall_child_style.display = Display::Block;
  tall_child_style.height = Some(Length::px(100.0));
  let tall_child = block_with_style(tall_child_style, vec![]);

  let mut auto_style = ComputedStyle::default();
  auto_style.display = Display::Block;
  auto_style.content_visibility = ContentVisibility::Auto;
  // No height and no explicit contain-intrinsic fallback => not stable, so layout should not skip.
  let auto_box = block_with_style(auto_style, vec![tall_child]);

  let root = block_with_style(ComputedStyle::default(), vec![spacer, auto_box]);
  let constraints = LayoutConstraints::new(AvailableSpace::Definite(200.0), AvailableSpace::Indefinite);
  let fragment = fc.layout(&root, &constraints).expect("layout");

  let auto_fragment = fragment
    .children
    .get(1)
    .expect("expected second child fragment");

  // The auto subtree is offscreen, but without a stable size it must still lay out descendants.
  assert!(
    (auto_fragment.bounds.height() - 100.0).abs() < 0.01,
    "auto subtree should retain its intrinsic height when not skipped (got {})",
    auto_fragment.bounds.height()
  );
  assert!(
    !auto_fragment.children.is_empty(),
    "auto subtree descendants should be laid out when block-size is unknown"
  );
}

#[test]
fn content_visibility_auto_skips_with_explicit_intrinsic_fallback() {
  let viewport = Size::new(200.0, 200.0);
  let fc = BlockFormattingContext::with_font_context_and_viewport(FontContext::new(), viewport);

  let mut spacer_style = ComputedStyle::default();
  spacer_style.display = Display::Block;
  spacer_style.height = Some(Length::px(240.0));
  let spacer = block_with_style(spacer_style, vec![]);

  let mut tall_child_style = ComputedStyle::default();
  tall_child_style.display = Display::Block;
  tall_child_style.height = Some(Length::px(100.0));
  let tall_child = block_with_style(tall_child_style, vec![]);

  let mut auto_style = ComputedStyle::default();
  auto_style.display = Display::Block;
  auto_style.content_visibility = ContentVisibility::Auto;
  auto_style.contain_intrinsic_height = ContainIntrinsicSizeAxis {
    auto: true,
    length: Some(Length::px(50.0)),
  };
  let auto_box = block_with_style(auto_style, vec![tall_child]);

  let root = block_with_style(ComputedStyle::default(), vec![spacer, auto_box]);
  let constraints = LayoutConstraints::new(AvailableSpace::Definite(200.0), AvailableSpace::Indefinite);
  let fragment = fc.layout(&root, &constraints).expect("layout");

  let auto_fragment = fragment
    .children
    .get(1)
    .expect("expected second child fragment");

  // With an explicit intrinsic fallback, the subtree can be skipped for layout and should size
  // itself using the fallback value.
  assert!(
    (auto_fragment.bounds.height() - 50.0).abs() < 0.01,
    "auto subtree should use contain-intrinsic fallback height when skipped (got {})",
    auto_fragment.bounds.height()
  );
  assert!(
    auto_fragment.children.is_empty(),
    "auto subtree descendants should be skipped when an explicit intrinsic fallback exists"
  );
}

