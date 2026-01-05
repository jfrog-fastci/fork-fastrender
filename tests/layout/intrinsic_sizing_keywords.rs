use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::formatting_context::IntrinsicSizingMode;
use fastrender::style::display::Display;
use fastrender::style::types::IntrinsicSizeKeyword;
use fastrender::style::values::Length;
use fastrender::BoxNode;
use fastrender::ComputedStyle;
use fastrender::FormattingContextFactory;
use fastrender::FormattingContextType;
use std::sync::Arc;

fn default_style() -> Arc<ComputedStyle> {
  Arc::new(ComputedStyle::default())
}

fn assert_approx(val: f32, expected: f32, msg: &str) {
  assert!(
    (val - expected).abs() <= 0.5,
    "{}: got {} expected {}",
    msg,
    val,
    expected
  );
}

fn block_container(children: Vec<BoxNode>) -> BoxNode {
  let mut style = ComputedStyle::default();
  style.display = Display::Block;
  BoxNode::new_block(Arc::new(style), FormattingContextType::Block, children)
}

#[test]
fn width_max_content_uses_intrinsic_max_inline_size() {
  let factory = FormattingContextFactory::new();
  let ctx = factory.create(FormattingContextType::Block);

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = None;
  child_style.width_keyword = Some(IntrinsicSizeKeyword::MaxContent);

  let child = BoxNode::new_block(
    Arc::new(child_style),
    FormattingContextType::Block,
    vec![BoxNode::new_text(default_style(), "Hello world".into())],
  );

  let expected = ctx
    .compute_intrinsic_inline_size(&child, IntrinsicSizingMode::MaxContent)
    .expect("intrinsic max-content width");

  let root = block_container(vec![child]);
  let fragment = ctx
    .layout(
      &root,
      &LayoutConstraints::definite_width(expected + 200.0),
    )
    .expect("layout");

  let child_fragment = &fragment.children[0];
  assert_approx(
    child_fragment.bounds.width(),
    expected,
    "max-content width should resolve to intrinsic max-content size",
  );
}

#[test]
fn width_fit_content_clamps_to_available_and_max_content() {
  let factory = FormattingContextFactory::new();
  let ctx = factory.create(FormattingContextType::Block);

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = None;
  child_style.width_keyword = Some(IntrinsicSizeKeyword::FitContent { limit: None });

  let child = BoxNode::new_block(
    Arc::new(child_style),
    FormattingContextType::Block,
    vec![BoxNode::new_text(default_style(), "lorem ipsum dolor".into())],
  );

  let (min_content, max_content) = ctx
    .compute_intrinsic_inline_sizes(&child)
    .expect("intrinsic sizes");
  assert!(
    max_content > min_content + 1.0,
    "expected max-content ({}) > min-content ({}) for fit-content test",
    max_content,
    min_content
  );

  // When the available width falls between min/max intrinsic widths, fit-content should resolve
  // to the available width.
  let available_mid = (min_content + max_content) / 2.0;
  assert!(available_mid > min_content && available_mid < max_content);
  let root = block_container(vec![child]);
  let fragment = ctx
    .layout(&root, &LayoutConstraints::definite_width(available_mid))
    .expect("layout");
  let child_fragment = &fragment.children[0];
  assert_approx(
    child_fragment.bounds.width(),
    available_mid,
    "fit-content keyword should clamp to available width when between intrinsic limits",
  );

  // When the available width exceeds max-content, fit-content should resolve to max-content.
  let available_large = max_content + 50.0;
  let fragment = ctx
    .layout(&root, &LayoutConstraints::definite_width(available_large))
    .expect("layout");
  let child_fragment = &fragment.children[0];
  assert_approx(
    child_fragment.bounds.width(),
    max_content,
    "fit-content keyword should resolve to max-content when available exceeds max-content",
  );
}

#[test]
fn intrinsic_keyword_constraints_clamp_used_width() {
  let factory = FormattingContextFactory::new();
  let ctx = factory.create(FormattingContextType::Block);

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = None;
  child_style.width_keyword = Some(IntrinsicSizeKeyword::MaxContent);
  child_style.max_width = None;
  child_style.max_width_keyword = Some(IntrinsicSizeKeyword::MinContent);

  // Compute the unconstrained intrinsic widths for the content so we can assert the clamp is
  // actually changing the used width.
  let mut measure_style = ComputedStyle::default();
  measure_style.display = Display::Block;
  let measure = BoxNode::new_block(
    Arc::new(measure_style),
    FormattingContextType::Block,
    vec![BoxNode::new_text(default_style(), "lorem ipsum dolor".into())],
  );
  let min_content = ctx
    .compute_intrinsic_inline_size(&measure, IntrinsicSizingMode::MinContent)
    .expect("intrinsic min-content width");
  let max_content = ctx
    .compute_intrinsic_inline_size(&measure, IntrinsicSizingMode::MaxContent)
    .expect("intrinsic max-content width");
  assert!(
    max_content > min_content + 1.0,
    "expected max-content ({}) > min-content ({})",
    max_content,
    min_content
  );

  let child = BoxNode::new_block(
    Arc::new(child_style),
    FormattingContextType::Block,
    vec![BoxNode::new_text(default_style(), "lorem ipsum dolor".into())],
  );

  let root = block_container(vec![child]);
  let fragment = ctx
    .layout(
      &root,
      &LayoutConstraints::definite_width(max_content + 200.0),
    )
    .expect("layout");
  let child_fragment = &fragment.children[0];
  assert_approx(
    child_fragment.bounds.width(),
    min_content,
    "max-width: min-content should clamp width: max-content",
  );
}

#[test]
fn height_max_content_includes_percentage_padding() {
  let factory = FormattingContextFactory::new();
  let ctx = factory.create(FormattingContextType::Block);

  let mut base_style = ComputedStyle::default();
  base_style.display = Display::Block;
  base_style.padding_top = Length::percent(10.0);
  base_style.padding_bottom = Length::percent(10.0);

  let auto_child = BoxNode::new_block(
    Arc::new(base_style.clone()),
    FormattingContextType::Block,
    vec![BoxNode::new_text(default_style(), "Hello".into())],
  );

  let mut intrinsic_style = base_style;
  intrinsic_style.height_keyword = Some(IntrinsicSizeKeyword::MaxContent);
  let intrinsic_child = BoxNode::new_block(
    Arc::new(intrinsic_style),
    FormattingContextType::Block,
    vec![BoxNode::new_text(default_style(), "Hello".into())],
  );

  // Padding percentages resolve against the containing block *width* (here: 200px → 20px each).
  let root = block_container(vec![auto_child, intrinsic_child]);
  let fragment = ctx
    .layout(&root, &LayoutConstraints::definite_width(200.0))
    .expect("layout");

  let auto_fragment = &fragment.children[0];
  let intrinsic_fragment = &fragment.children[1];
  assert!(
    auto_fragment.bounds.height() > 40.0,
    "auto height should include content height in addition to percentage padding"
  );
  assert_approx(
    intrinsic_fragment.bounds.height(),
    auto_fragment.bounds.height(),
    "height: max-content should include percentage padding resolved against the container width",
  );
}
