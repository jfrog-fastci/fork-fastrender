use fastrender::geometry::Rect;
use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::style::float::{Clear, Float};
use fastrender::style::values::Length;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use fastrender::{BoxNode, ComputedStyle, FastRender, FontConfig, FormattingContext, FormattingContextType};
use std::sync::Arc;

fn find_block_with_line_children_and_width<'a>(
  node: &'a FragmentNode,
  target_width: f32,
) -> Option<&'a FragmentNode> {
  if matches!(node.content, FragmentContent::Block { .. })
    && (node.bounds.width() - target_width).abs() < 0.5
    && node
      .children
      .iter()
      .any(|child| matches!(child.content, FragmentContent::Line { .. }))
  {
    return Some(node);
  }
  for child in node.children.iter() {
    if let Some(found) = find_block_with_line_children_and_width(child, target_width) {
      return Some(found);
    }
  }
  None
}

fn line_bounds_for(html: &str, block_width: f32) -> Vec<Rect> {
  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer");

  let dom = renderer.parse_html(html).expect("parse HTML");
  let fragments = renderer
    .layout_document(&dom, 200, 200)
    .expect("layout document");

  let block = find_block_with_line_children_and_width(&fragments.root, block_width)
    .expect("expected a block fragment with line children");

  block
    .children
    .iter()
    .filter(|child| matches!(child.content, FragmentContent::Line { .. }))
    .map(|line| line.bounds)
    .collect()
}

#[test]
fn line_boxes_shorten_next_to_left_float_and_expand_after_float_ends() {
  let html = "<!doctype html><style>\
    body{margin:0;}\
    #wrap{width:100px;}\
    .float{float:left;width:60px;height:20px;background:#00f;}\
    .para{font-size:0;line-height:10px;}\
    .piece{display:inline-block;width:30px;height:10px;background:#0f0;vertical-align:top;}\
  </style>\
  <div id=wrap><div class=float></div><div class=para><span class=piece></span><span class=piece></span><span class=piece></span></div></div>";

  let lines = line_bounds_for(html, 100.0);
  assert_eq!(lines.len(), 3);

  // The first two lines overlap the float's vertical span (0-20), so they are shifted right and
  // narrowed. The third line begins at y=20 and should have full width.
  assert!((lines[0].x() - 60.0).abs() < 0.5);
  assert!((lines[0].y() - 0.0).abs() < 0.5);
  assert!((lines[0].width() - 40.0).abs() < 0.5);

  assert!((lines[1].x() - 60.0).abs() < 0.5);
  assert!((lines[1].y() - 10.0).abs() < 0.5);
  assert!((lines[1].width() - 40.0).abs() < 0.5);

  assert!((lines[2].x() - 0.0).abs() < 0.5);
  assert!((lines[2].y() - 20.0).abs() < 0.5);
  assert!((lines[2].width() - 100.0).abs() < 0.5);
}

#[test]
fn line_boxes_push_below_left_float_when_no_horizontal_space_remains() {
  let html = "<!doctype html><style>\
    body{margin:0;}\
    #wrap{width:100px;}\
    .float{float:left;width:90px;height:20px;background:#00f;}\
    .para{font-size:0;line-height:10px;}\
    .piece{display:inline-block;width:30px;height:10px;background:#0f0;vertical-align:top;}\
  </style>\
  <div id=wrap><div class=float></div><div class=para><span class=piece></span></div></div>";

  let lines = line_bounds_for(html, 100.0);
  assert_eq!(lines.len(), 1);

  // Only 10px remains next to the float, so the line is pushed below the float's 20px height.
  assert!((lines[0].x() - 0.0).abs() < 0.5);
  assert!((lines[0].y() - 20.0).abs() < 0.5);
  assert!((lines[0].width() - 100.0).abs() < 0.5);
}

#[test]
fn line_boxes_push_below_float_for_inline_replaced_elements() {
  // Same scenario as `line_boxes_push_below_left_float_when_no_horizontal_space_remains`, but use
  // an inline replaced element (`<canvas>`) instead of an inline-block. Replaced elements can't be
  // split across lines, so the empty line must be pushed below the float until it fits.
  let html = "<!doctype html><style>\
    body{margin:0;}\
    #wrap{width:100px;}\
    .float{float:left;width:90px;height:20px;background:#00f;}\
    .para{font-size:0;line-height:10px;}\
    canvas{width:30px;height:10px;background:#0f0;vertical-align:top;}\
  </style>\
  <div id=wrap><div class=float></div><div class=para><canvas></canvas></div></div>";

  let lines = line_bounds_for(html, 100.0);
  assert_eq!(lines.len(), 1);

  assert!((lines[0].x() - 0.0).abs() < 0.5);
  assert!((lines[0].y() - 20.0).abs() < 0.5);
  assert!((lines[0].width() - 100.0).abs() < 0.5);
}

#[test]
fn line_boxes_shorten_next_to_right_float_over_entire_line_height() {
  let html = "<!doctype html><style>\
    body{margin:0;}\
    #wrap{width:100px;}\
    .float{float:right;width:60px;height:15px;background:#00f;}\
    .para{font-size:0;line-height:10px;}\
    .piece{display:inline-block;width:30px;height:10px;background:#0f0;vertical-align:top;}\
  </style>\
  <div id=wrap><div class=float></div><div class=para><span class=piece></span><span class=piece></span><span class=piece></span></div></div>";

  let lines = line_bounds_for(html, 100.0);
  assert_eq!(lines.len(), 3);

  // The float overlaps the first line (0-10) and partially overlaps the second (10-20). Range
  // queries should use the most constrained width for the entire line box, so both lines are
  // shortened to 40px. The third line starts at y=20 and has the full width.
  assert!((lines[0].x() - 0.0).abs() < 0.5);
  assert!((lines[0].y() - 0.0).abs() < 0.5);
  assert!((lines[0].width() - 40.0).abs() < 0.5);

  assert!((lines[1].x() - 0.0).abs() < 0.5);
  assert!((lines[1].y() - 10.0).abs() < 0.5);
  assert!((lines[1].width() - 40.0).abs() < 0.5);

  assert!((lines[2].x() - 0.0).abs() < 0.5);
  assert!((lines[2].y() - 20.0).abs() < 0.5);
  assert!((lines[2].width() - 100.0).abs() < 0.5);
}

#[test]
fn clear_breaks_margin_collapse_and_pushes_below_float() {
  // Container with a float and a following block that clears it. The clearing block's margin-top
  // must not collapse through the parent once clearance is introduced.
  let container_style = Arc::new(ComputedStyle::default());

  let mut float_style = ComputedStyle::default();
  float_style.float = Float::Left;
  float_style.width = Some(Length::px(50.0));
  float_style.height = Some(Length::px(50.0));
  let float_box = BoxNode::new_block(Arc::new(float_style), FormattingContextType::Block, vec![]);

  let mut clear_style = ComputedStyle::default();
  clear_style.clear = Clear::Left;
  clear_style.margin_top = Some(Length::px(20.0));
  clear_style.height = Some(Length::px(10.0));
  let clear_box = BoxNode::new_block(Arc::new(clear_style), FormattingContextType::Block, vec![]);

  let container = BoxNode::new_block(
    container_style,
    FormattingContextType::Block,
    vec![float_box, clear_box],
  );

  let bfc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(200.0, 1000.0);
  let fragment = bfc.layout(&container, &constraints).expect("layout should succeed");

  assert_eq!(fragment.children.len(), 2);
  let float_fragment = &fragment.children[0];
  let clear_fragment = &fragment.children[1];

  assert!((float_fragment.bounds.y() - 0.0).abs() < 0.5);
  assert!((float_fragment.bounds.height() - 50.0).abs() < 0.5);

  // Float ends at y=50, and the clearing block has a 20px top margin that should not collapse with
  // the parent once clearance is applied. Expect border-box y = 50 + 20.
  assert!(
    (clear_fragment.bounds.y() - 70.0).abs() < 0.5,
    "expected clear block at y≈70, got {:.2}",
    clear_fragment.bounds.y()
  );
}
