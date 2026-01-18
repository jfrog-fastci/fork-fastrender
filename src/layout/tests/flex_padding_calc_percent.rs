use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::flex::FlexFormattingContext;
use crate::style::display::Display;
use crate::style::display::FormattingContextType;
use crate::style::types::WritingMode;
use crate::style::values::CalcLength;
use crate::style::values::Length;
use crate::style::values::LengthUnit;
use crate::tree::fragment_tree::{FragmentContent, FragmentNode};
use crate::{BoxNode, ComputedStyle, FormattingContext};
use std::sync::Arc;

fn find_child_by_id<'a>(fragment: &'a FragmentNode, id: usize) -> Option<&'a FragmentNode> {
  fragment.children.iter().find(|child| {
    matches!(
      child.content,
      FragmentContent::Block { box_id: Some(box_id) }
        | FragmentContent::Inline { box_id: Some(box_id), .. }
        | FragmentContent::Text { box_id: Some(box_id), .. }
        | FragmentContent::Replaced { box_id: Some(box_id), .. }
        if box_id == id
    )
  })
}

#[test]
fn flex_padding_calc_with_percentage_resolves_against_containing_block() {
  // Regression test for `padding-left/right: calc(<percentage> + <length>)` on a flex container.
  //
  // Previously the `calc()` term containing a percentage was dropped to 0 during Taffy conversion,
  // causing all in-flow children to start at x=0. On abcnews.go.com this broke centered column
  // wrappers using `padding-left/right: calc(50% - <half-column-width>)`.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.width = Some(Length::px(200.0));
  container_style.height = Some(Length::px(50.0));

  let calc = |percent: f32, px: f32| -> Length {
    let calc = CalcLength::single(LengthUnit::Percent, percent)
      .add_scaled(&CalcLength::single(LengthUnit::Px, px), 1.0)
      .expect("calc expression should be representable");
    Length::calc(calc)
  };

  container_style.padding_left = calc(50.0, -40.0);
  container_style.padding_right = calc(50.0, -40.0);

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));
  child_style.flex_shrink = 0.0;
  let mut child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
  child.id = 1;

  let mut container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![child],
  );
  container.id = 100;

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(200.0, 50.0))
    .expect("layout succeeds");

  let child_frag = find_child_by_id(&fragment, 1).expect("child fragment");
  // 50% of 200px is 100px; minus 40px => 60px padding.
  assert!(
    (child_frag.bounds.x() - 60.0).abs() < 0.5,
    "expected child x≈60, got x={}",
    child_frag.bounds.x()
  );
}

#[test]
fn flex_padding_calc_with_percentage_resolves_against_physical_width_in_vertical_writing_mode() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.writing_mode = WritingMode::VerticalRl;
  container_style.width = Some(Length::px(200.0));
  container_style.height = Some(Length::px(50.0));

  let calc = |percent: f32, px: f32| -> Length {
    let calc = CalcLength::single(LengthUnit::Percent, percent)
      .add_scaled(&CalcLength::single(LengthUnit::Px, px), 1.0)
      .expect("calc expression should be representable");
    Length::calc(calc)
  };

  container_style.padding_left = calc(50.0, -40.0);
  container_style.padding_right = calc(50.0, -40.0);

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));
  child_style.flex_shrink = 0.0;
  let mut child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
  child.id = 1;

  let mut container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![child],
  );
  container.id = 100;

  // In vertical writing modes the containing block's physical width is the **block** axis.
  let constraints =
    LayoutConstraints::definite(80.0, 200.0).with_block_percentage_base(Some(200.0));
  let fc = FlexFormattingContext::new();
  let fragment = fc.layout(&container, &constraints).expect("layout succeeds");

  let child_frag = find_child_by_id(&fragment, 1).expect("child fragment");
  assert!(
    (child_frag.bounds.x() - 60.0).abs() < 0.5,
    "expected child x≈60, got x={}",
    child_frag.bounds.x()
  );
}
