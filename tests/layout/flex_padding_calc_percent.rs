use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::values::CalcLength;
use fastrender::style::values::Length;
use fastrender::style::values::LengthUnit;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use fastrender::{BoxNode, ComputedStyle, FormattingContext};
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

