use fastrender::style::types::LineHeight;
use fastrender::text::font_db::FontConfig;
use fastrender::text::font_loader::FontContext;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use fastrender::{
  BoxNode, BoxTree, ComputedStyle, Display, Float, FormattingContextType, LayoutConfig,
  LayoutEngine, Length, Rgba, Size,
};
use std::sync::Arc;

fn find_fragment_by_background<'a>(
  node: &'a FragmentNode,
  color: Rgba,
) -> Option<&'a FragmentNode> {
  if node
    .style
    .as_ref()
    .is_some_and(|style| style.background_color == color)
  {
    return Some(node);
  }
  for child in node.children.iter() {
    if let Some(found) = find_fragment_by_background(child, color) {
      return Some(found);
    }
  }
  None
}

fn fragment_contains_background(node: &FragmentNode, color: Rgba) -> bool {
  if node
    .style
    .as_ref()
    .is_some_and(|style| style.background_color == color)
  {
    return true;
  }
  node
    .children
    .iter()
    .any(|child| fragment_contains_background(child, color))
}

fn find_line_containing_background<'a>(
  node: &'a FragmentNode,
  color: Rgba,
) -> Option<&'a FragmentNode> {
  if matches!(node.content, FragmentContent::Line { .. })
    && fragment_contains_background(node, color)
  {
    return Some(node);
  }
  for child in node.children.iter() {
    if let Some(found) = find_line_containing_background(child, color) {
      return Some(found);
    }
  }
  None
}

fn line_baseline(line: &FragmentNode) -> f32 {
  match line.content {
    FragmentContent::Line { baseline } => baseline,
    _ => panic!("expected FragmentContent::Line, got {:?}", line.content),
  }
}

#[test]
fn inline_block_baseline_falls_back_to_bottom_when_only_float_descendants_have_lines() {
  // Regression test for facebook.com: inline-block baselines use the last *in-flow* line box.
  // Floats are out-of-flow (CSS 2.1 §9.3.1), so line boxes inside floated descendants must not
  // contribute, otherwise the inline-block incorrectly stops falling back to the bottom margin edge.

  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;

  let mut inline_fc_style = root_style.clone();
  inline_fc_style.font_size = 16.0;
  inline_fc_style.line_height = LineHeight::Length(Length::px(16.0));

  let mut inline_block_style = inline_fc_style.clone();
  inline_block_style.display = Display::InlineBlock;
  inline_block_style.width = Some(Length::px(120.0));
  inline_block_style.height = Some(Length::px(80.0));
  inline_block_style.background_color = Rgba::rgb(1, 2, 3);

  let mut float_style = inline_fc_style.clone();
  float_style.display = Display::Block;
  float_style.float = Float::Left;
  float_style.font_size = 48.0;
  float_style.line_height = LineHeight::Length(Length::px(48.0));

  let float_text = BoxNode::new_text(Arc::new(float_style.clone()), "X".to_string());
  let float_box = BoxNode::new_block(
    Arc::new(float_style),
    FormattingContextType::Block,
    vec![float_text],
  );
  let inline_block = BoxNode::new_inline_block(
    Arc::new(inline_block_style),
    FormattingContextType::Block,
    vec![float_box],
  );

  let inline_fc = BoxNode::new_block(
    Arc::new(inline_fc_style),
    FormattingContextType::Inline,
    vec![inline_block],
  );
  let root = BoxNode::new_block(
    Arc::new(root_style),
    FormattingContextType::Block,
    vec![inline_fc],
  );
  let tree = BoxTree::new(root);

  let config = LayoutConfig::for_viewport(Size::new(400.0, 240.0));
  let font_context = FontContext::with_config(FontConfig::bundled_only());
  let engine = LayoutEngine::with_font_context(config, font_context);
  let fragments = engine.layout_tree(&tree).expect("layout tree");

  let target = Rgba::rgb(1, 2, 3);
  let line = find_line_containing_background(&fragments.root, target).expect("line fragment");
  let inline_block_fragment =
    find_fragment_by_background(&fragments.root, target).expect("inline-block fragment");

  let baseline = line_baseline(line);
  let height = inline_block_fragment.bounds.height();

  assert!(
    (baseline - height).abs() <= 0.5,
    "expected inline-block baseline to fall back to bottom edge when only floats have line boxes; baseline={baseline:.2} height={height:.2}",
  );
  assert!(
    line.bounds.height() > height,
    "expected line box to include strut descent below bottom-edge baseline; line_h={:.2} inline_block_h={height:.2}",
    line.bounds.height(),
  );
}

#[test]
fn inline_block_baseline_ignores_float_line_boxes_after_in_flow_content() {
  // If an inline-block has both in-flow line boxes and floated descendants later in tree order,
  // the baseline must come from the last *in-flow* line box (not the float).

  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;

  let mut inline_fc_style = root_style.clone();
  inline_fc_style.font_size = 16.0;
  inline_fc_style.line_height = LineHeight::Length(Length::px(16.0));

  let mut ib_base_style = inline_fc_style.clone();
  ib_base_style.display = Display::InlineBlock;
  ib_base_style.width = Some(Length::px(140.0));
  ib_base_style.height = Some(Length::px(120.0));

  let mut in_flow_block_style = inline_fc_style.clone();
  in_flow_block_style.display = Display::Block;
  in_flow_block_style.font_size = 16.0;
  in_flow_block_style.line_height = LineHeight::Length(Length::px(16.0));

  let mut float_style = inline_fc_style.clone();
  float_style.display = Display::Block;
  float_style.float = Float::Left;
  float_style.font_size = 72.0;
  float_style.line_height = LineHeight::Length(Length::px(72.0));

  let make_inline_block = |background: Rgba, include_float: bool| -> BoxNode {
    let mut style = ib_base_style.clone();
    style.background_color = background;

    let text = BoxNode::new_text(Arc::new(in_flow_block_style.clone()), "x".to_string());
    let in_flow_block = BoxNode::new_block(
      Arc::new(in_flow_block_style.clone()),
      FormattingContextType::Block,
      vec![text],
    );

    let mut children = vec![in_flow_block];
    if include_float {
      let float_text = BoxNode::new_text(Arc::new(float_style.clone()), "Y".to_string());
      let float_box = BoxNode::new_block(
        Arc::new(float_style.clone()),
        FormattingContextType::Block,
        vec![float_text],
      );
      children.push(float_box);
    }

    BoxNode::new_inline_block(Arc::new(style), FormattingContextType::Block, children)
  };

  let ref_color = Rgba::rgb(10, 20, 30);
  let float_color = Rgba::rgb(40, 50, 60);

  let line_ref = BoxNode::new_block(
    Arc::new(inline_fc_style.clone()),
    FormattingContextType::Inline,
    vec![make_inline_block(ref_color, false)],
  );
  let line_with_float = BoxNode::new_block(
    Arc::new(inline_fc_style),
    FormattingContextType::Inline,
    vec![make_inline_block(float_color, true)],
  );

  let root = BoxNode::new_block(
    Arc::new(root_style),
    FormattingContextType::Block,
    vec![line_ref, line_with_float],
  );
  let tree = BoxTree::new(root);

  let config = LayoutConfig::for_viewport(Size::new(400.0, 400.0));
  let font_context = FontContext::with_config(FontConfig::bundled_only());
  let engine = LayoutEngine::with_font_context(config, font_context);
  let fragments = engine.layout_tree(&tree).expect("layout tree");

  let line_a = find_line_containing_background(&fragments.root, ref_color).expect("reference line");
  let line_b = find_line_containing_background(&fragments.root, float_color).expect("float line");

  let baseline_a = line_baseline(line_a);
  let baseline_b = line_baseline(line_b);

  assert!(
    (baseline_a - baseline_b).abs() <= 0.5,
    "expected floated descendants to not affect inline-block baseline; baseline_a={baseline_a:.2} baseline_b={baseline_b:.2}",
  );
}
