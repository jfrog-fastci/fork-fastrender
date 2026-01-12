use fastrender::geometry::Size;
use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::layout::contexts::positioned::ContainingBlock;
use fastrender::style::display::{Display, FormattingContextType};
use fastrender::style::position::Position;
use fastrender::style::types::InsetValue;
use fastrender::style::values::Length;
use fastrender::text::font_loader::FontContext;
use fastrender::tree::fragment_tree::FragmentContent;
use fastrender::{BoxNode, ComputedStyle, FormattingContext};
use std::sync::Arc;

#[test]
fn abspos_nested_bottom_inset_uses_containing_block_used_height() {
  // Regression test for nested abspos placement when the containing block is `height:auto`.
  //
  // Structure:
  //   root
  //     cb (position: relative; height: auto)
  //       wrap (position: static)
  //         sizer (height: 50px)   -> determines cb used height
  //         abs (position: absolute; bottom: 0; height: 10px)
  //
  // The abspos element's containing block is `cb`, but it is laid out while `cb`'s height is
  // unknown. We must relayout descendants once `cb`'s used height is known so `bottom: 0` places
  // the element at y = 50 - 10 = 40, not y = -10 (as if the containing block height were 0).

  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;

  let mut cb_style = ComputedStyle::default();
  cb_style.display = Display::Block;
  cb_style.position = Position::Relative;
  cb_style.width = Some(Length::px(100.0));

  let mut wrap_style = ComputedStyle::default();
  wrap_style.display = Display::Block;

  let mut sizer_style = ComputedStyle::default();
  sizer_style.display = Display::Block;
  sizer_style.height = Some(Length::px(50.0));
  let mut sizer = BoxNode::new_block(Arc::new(sizer_style), FormattingContextType::Block, vec![]);
  sizer.id = 4;

  let mut abs_style = ComputedStyle::default();
  abs_style.display = Display::Block;
  abs_style.position = Position::Absolute;
  abs_style.left = InsetValue::Length(Length::px(0.0));
  abs_style.bottom = InsetValue::Length(Length::px(0.0));
  abs_style.width = Some(Length::px(100.0));
  abs_style.height = Some(Length::px(10.0));
  let mut abs_child = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
  abs_child.id = 5;

  let mut wrap = BoxNode::new_block(
    Arc::new(wrap_style),
    FormattingContextType::Block,
    vec![sizer, abs_child],
  );
  wrap.id = 3;

  let mut cb = BoxNode::new_block(Arc::new(cb_style), FormattingContextType::Block, vec![wrap]);
  cb.id = 2;

  let mut root = BoxNode::new_block(Arc::new(root_style), FormattingContextType::Block, vec![cb]);
  root.id = 1;

  let bfc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(100.0, 200.0);
  let fragment = bfc.layout(&root, &constraints).expect("layout succeeds");

  let abs_fragment = fragment
    .iter_fragments()
    .find(|f| {
      matches!(
        f.content,
        FragmentContent::Block { box_id: Some(box_id) }
          | FragmentContent::Inline { box_id: Some(box_id), .. }
          | FragmentContent::Text { box_id: Some(box_id), .. }
          | FragmentContent::Replaced { box_id: Some(box_id), .. }
          if box_id == 5
      )
    })
    .unwrap_or_else(|| panic!("missing abspos fragment in tree: {fragment:#?}"));

  assert!(
    (abs_fragment.bounds.y() - 40.0).abs() < 0.5,
    "expected `bottom: 0` to use the containing block's used height (expected y=40, got y={})",
    abs_fragment.bounds.y()
  );
}

#[test]
fn abspos_nested_bottom_inset_uses_containing_block_used_height_for_root_layout() {
  // Same as the test above, but exercises the root `BlockFormattingContext::layout` entrypoint
  // (used for flex/grid items) rather than `layout_block_child`.

  let mut cb_style = ComputedStyle::default();
  cb_style.display = Display::Block;
  cb_style.position = Position::Relative;
  cb_style.width = Some(Length::px(100.0));

  let mut wrap_style = ComputedStyle::default();
  wrap_style.display = Display::Block;

  let mut sizer_style = ComputedStyle::default();
  sizer_style.display = Display::Block;
  sizer_style.height = Some(Length::px(50.0));
  let mut sizer = BoxNode::new_block(Arc::new(sizer_style), FormattingContextType::Block, vec![]);
  sizer.id = 3;

  let mut abs_style = ComputedStyle::default();
  abs_style.display = Display::Block;
  abs_style.position = Position::Absolute;
  abs_style.left = InsetValue::Length(Length::px(0.0));
  abs_style.bottom = InsetValue::Length(Length::px(0.0));
  abs_style.width = Some(Length::px(100.0));
  abs_style.height = Some(Length::px(10.0));
  let mut abs_child = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
  abs_child.id = 4;

  let mut wrap = BoxNode::new_block(
    Arc::new(wrap_style),
    FormattingContextType::Block,
    vec![sizer, abs_child],
  );
  wrap.id = 2;

  let mut cb = BoxNode::new_block(Arc::new(cb_style), FormattingContextType::Block, vec![wrap]);
  cb.id = 1;

  let viewport = Size::new(800.0, 600.0);
  let bfc = BlockFormattingContext::for_flex_item_with_font_context_viewport_and_cb(
    FontContext::new(),
    viewport,
    ContainingBlock::viewport(viewport),
  );
  let constraints = LayoutConstraints::definite(100.0, 200.0);
  let fragment = bfc.layout(&cb, &constraints).expect("layout succeeds");

  let abs_fragment = fragment
    .iter_fragments()
    .find(|f| {
      matches!(
        f.content,
        FragmentContent::Block { box_id: Some(box_id) }
          | FragmentContent::Inline { box_id: Some(box_id), .. }
          | FragmentContent::Text { box_id: Some(box_id), .. }
          | FragmentContent::Replaced { box_id: Some(box_id), .. }
          if box_id == 4
      )
    })
    .unwrap_or_else(|| panic!("missing abspos fragment in tree: {fragment:#?}"));

  assert!(
    (abs_fragment.bounds.y() - 40.0).abs() < 0.5,
    "expected `bottom: 0` to use the containing block's used height (expected y=40, got y={})",
    abs_fragment.bounds.y()
  );
}
