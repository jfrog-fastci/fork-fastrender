use fastrender::layout::constraints::AvailableSpace;
use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::types::FlexBasis;
use fastrender::style::types::FlexDirection;
use fastrender::style::values::Length;
use fastrender::BoxNode;
use fastrender::ComputedStyle;
use fastrender::FormattingContextType;
use fastrender::FragmentNode;
use std::sync::Arc;

fn find_first_fragment_with_id<'a>(fragment: &'a FragmentNode, id: usize) -> Option<&'a FragmentNode> {
  if fragment.box_id().is_some_and(|fragment_id| fragment_id == id) {
    return Some(fragment);
  }
  for child in fragment.children.iter() {
    if let Some(found) = find_first_fragment_with_id(child, id) {
      return Some(found);
    }
  }
  None
}

fn assert_approx(value: f32, expected: f32, msg: &str) {
  assert!(
    (value - expected).abs() <= 0.5,
    "{msg}: got {value:.2} expected {expected:.2}"
  );
}

#[test]
fn flex_basis_content_ignores_preferred_main_size() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.width = Some(Length::px(200.0));
  let container_style = Arc::new(container_style);

  let mut child_a_style = ComputedStyle::default();
  child_a_style.display = Display::Block;
  child_a_style.width = Some(Length::px(100.0));
  child_a_style.flex_grow = 0.0;
  child_a_style.flex_shrink = 0.0;
  child_a_style.flex_basis = FlexBasis::Content;
  let child_a_style = Arc::new(child_a_style);

  let mut child_b_style = ComputedStyle::default();
  child_b_style.display = Display::Block;
  child_b_style.width = Some(Length::px(50.0));
  child_b_style.flex_grow = 0.0;
  child_b_style.flex_shrink = 0.0;
  child_b_style.flex_basis = FlexBasis::Auto;
  let child_b_style = Arc::new(child_b_style);

  let mut child_a = BoxNode::new_block(child_a_style, FormattingContextType::Block, vec![]);
  child_a.id = 1;
  let mut child_b = BoxNode::new_block(child_b_style, FormattingContextType::Block, vec![]);
  child_b.id = 2;

  let mut container = BoxNode::new_block(
    container_style,
    FormattingContextType::Flex,
    vec![child_a, child_b],
  );
  container.id = 3;

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(
      &container,
      &LayoutConstraints::new(AvailableSpace::Definite(200.0), AvailableSpace::Indefinite),
    )
    .expect("layout succeeds");

  let child_a_fragment = find_first_fragment_with_id(&fragment, 1).expect("child A fragment");
  let child_b_fragment = find_first_fragment_with_id(&fragment, 2).expect("child B fragment");

  assert_approx(child_a_fragment.bounds.width(), 0.0, "child A should size to its content");
  assert_approx(
    child_b_fragment.bounds.width(),
    50.0,
    "child B should size using its preferred width",
  );
  assert_approx(
    child_b_fragment.bounds.x(),
    0.0,
    "child B should be positioned at the container start",
  );
}

#[test]
fn flex_basis_content_ignores_preferred_main_size_in_column_flex() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Column;
  container_style.height = Some(Length::px(200.0));
  let container_style = Arc::new(container_style);

  let mut child_a_style = ComputedStyle::default();
  child_a_style.display = Display::Block;
  child_a_style.height = Some(Length::px(100.0));
  child_a_style.flex_grow = 0.0;
  child_a_style.flex_shrink = 0.0;
  child_a_style.flex_basis = FlexBasis::Content;
  let child_a_style = Arc::new(child_a_style);

  let mut child_b_style = ComputedStyle::default();
  child_b_style.display = Display::Block;
  child_b_style.height = Some(Length::px(50.0));
  child_b_style.flex_grow = 0.0;
  child_b_style.flex_shrink = 0.0;
  child_b_style.flex_basis = FlexBasis::Auto;
  let child_b_style = Arc::new(child_b_style);

  let mut child_a = BoxNode::new_block(child_a_style, FormattingContextType::Block, vec![]);
  child_a.id = 10;
  let mut child_b = BoxNode::new_block(child_b_style, FormattingContextType::Block, vec![]);
  child_b.id = 11;

  let mut container = BoxNode::new_block(
    container_style,
    FormattingContextType::Flex,
    vec![child_a, child_b],
  );
  container.id = 12;

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(200.0, 200.0))
    .expect("layout succeeds");

  let child_a_fragment = find_first_fragment_with_id(&fragment, 10).expect("child A fragment");
  let child_b_fragment = find_first_fragment_with_id(&fragment, 11).expect("child B fragment");

  assert_approx(
    child_a_fragment.bounds.height(),
    0.0,
    "child A should size to its content on the main axis",
  );
  assert_approx(
    child_b_fragment.bounds.height(),
    50.0,
    "child B should size using its preferred height",
  );
  assert_approx(
    child_b_fragment.bounds.y(),
    0.0,
    "child B should be positioned at the container start",
  );
}

#[test]
fn flex_basis_content_uses_max_content_size_for_base_size() {
  let text = "lorem ipsum dolor sit amet";

  let block_fc = BlockFormattingContext::new();
  let measure = BoxNode::new_block(
    Arc::new(ComputedStyle::default()),
    FormattingContextType::Block,
    vec![BoxNode::new_text(
      Arc::new(ComputedStyle::default()),
      text.to_string(),
    )],
  );
  let (min_content, max_content) = block_fc
    .compute_intrinsic_inline_sizes(&measure)
    .expect("intrinsic inline sizes");
  assert!(
    max_content > min_content + 1.0,
    "expected max-content ({max_content:.2}) to exceed min-content ({min_content:.2})"
  );

  // Ensure the flex container is wide enough that the item doesn't shrink.
  let container_width = max_content + 50.0;
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.width = Some(Length::px(container_width));

  let mut item_style = ComputedStyle::default();
  item_style.display = Display::Block;
  // This preferred main-size should be ignored because flex-basis is `content`.
  item_style.width = Some(Length::px(10.0));
  item_style.flex_grow = 0.0;
  item_style.flex_shrink = 0.0;
  item_style.flex_basis = FlexBasis::Content;

  let mut item = BoxNode::new_block(
    Arc::new(item_style),
    FormattingContextType::Block,
    vec![BoxNode::new_text(
      Arc::new(ComputedStyle::default()),
      text.to_string(),
    )],
  );
  item.id = 20;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![item],
  );

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(
      &container,
      &LayoutConstraints::new(
        AvailableSpace::Definite(container_width),
        AvailableSpace::Indefinite,
      ),
    )
    .expect("layout succeeds");

  let item_fragment = find_first_fragment_with_id(&fragment, 20).expect("item fragment");
  assert_approx(
    item_fragment.bounds.width(),
    max_content,
    "item should size to its max-content width when flex-basis is content",
  );
}

#[test]
fn flex_basis_content_ignores_min_main_size_when_growing() {
  let text = "x y";

  let block_fc = BlockFormattingContext::new();
  let measure = BoxNode::new_block(
    Arc::new(ComputedStyle::default()),
    FormattingContextType::Block,
    vec![BoxNode::new_text(
      Arc::new(ComputedStyle::default()),
      text.to_string(),
    )],
  );
  let (_, max_content) = block_fc
    .compute_intrinsic_inline_sizes(&measure)
    .expect("intrinsic inline sizes");
  assert!(
    max_content < 100.0,
    "expected test text to have a small max-content width; got {max_content:.2}"
  );

  let container_width = 600.0;
  let fixed_width = 200.0;

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.width = Some(Length::px(container_width));

  let mut child_a_style = ComputedStyle::default();
  child_a_style.display = Display::Block;
  child_a_style.min_width = Some(Length::px(100.0));
  child_a_style.flex_grow = 1.0;
  child_a_style.flex_shrink = 0.0;
  child_a_style.flex_basis = FlexBasis::Content;

  let mut child_b_style = ComputedStyle::default();
  child_b_style.display = Display::Block;
  child_b_style.width = Some(Length::px(fixed_width));
  child_b_style.flex_grow = 1.0;
  child_b_style.flex_shrink = 0.0;
  child_b_style.flex_basis = FlexBasis::Auto;

  let mut child_a = BoxNode::new_block(
    Arc::new(child_a_style),
    FormattingContextType::Block,
    vec![BoxNode::new_text(
      Arc::new(ComputedStyle::default()),
      text.to_string(),
    )],
  );
  child_a.id = 30;
  let mut child_b = BoxNode::new_block(
    Arc::new(child_b_style),
    FormattingContextType::Block,
    vec![],
  );
  child_b.id = 31;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![child_a, child_b],
  );

  let free_space = container_width - (max_content + fixed_width);
  let expected_a = max_content + free_space / 2.0;
  let expected_b = fixed_width + free_space / 2.0;

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(
      &container,
      &LayoutConstraints::new(
        AvailableSpace::Definite(container_width),
        AvailableSpace::Indefinite,
      ),
    )
    .expect("layout succeeds");

  let child_a_fragment = find_first_fragment_with_id(&fragment, 30).expect("child A fragment");
  let child_b_fragment = find_first_fragment_with_id(&fragment, 31).expect("child B fragment");

  assert_approx(
    child_a_fragment.bounds.width(),
    expected_a,
    "child A should start from max-content (not min-width) when distributing free space",
  );
  assert_approx(
    child_b_fragment.bounds.width(),
    expected_b,
    "child B should receive the expected share of free space",
  );
}
