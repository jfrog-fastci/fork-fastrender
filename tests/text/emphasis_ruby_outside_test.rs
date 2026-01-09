use fastrender::paint::display_list::DisplayItem;
use fastrender::style::types::TextEmphasisPosition;
use fastrender::text::font_db::FontConfig;
use fastrender::tree::fragment_tree::FragmentContent;
use fastrender::tree::fragment_tree::FragmentNode;
use fastrender::tree::fragment_tree::TextEmphasisOffset;
use fastrender::{FastRender, Point, RenderArtifactRequest, RenderArtifacts, RenderOptions};

fn text_y_span(root: &FragmentNode, needle: &str) -> Option<(f32, f32)> {
  let mut min_y = f32::INFINITY;
  let mut max_y = f32::NEG_INFINITY;

  let mut stack: Vec<(&FragmentNode, Point)> = vec![(root, Point::ZERO)];
  while let Some((node, offset)) = stack.pop() {
    let abs_x = offset.x + node.bounds.x();
    let abs_y = offset.y + node.bounds.y();

    if let FragmentContent::Text { text, .. } = &node.content {
      if text.as_ref() == needle {
        min_y = min_y.min(abs_y);
        max_y = max_y.max(abs_y + node.bounds.height());
      }
    }

    let child_offset = Point::new(abs_x, abs_y);
    for child in node.children.iter().rev() {
      stack.push((child, child_offset));
    }
  }

  if min_y.is_finite() {
    Some((min_y, max_y))
  } else {
    None
  }
}

fn text_emphasis_offset(root: &FragmentNode, needle: &str) -> Option<TextEmphasisOffset> {
  let mut stack: Vec<&FragmentNode> = vec![root];
  while let Some(node) = stack.pop() {
    if let FragmentContent::Text {
      text,
      emphasis_offset,
      ..
    } = &node.content
    {
      if text.as_ref() == needle {
        return Some(*emphasis_offset);
      }
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  None
}

#[test]
fn text_emphasis_marks_render_outside_ruby_annotations_on_the_same_side() {
  let html = r#"<!doctype html><html><head><style>
    body{margin:0;font-size:40px;}
    /* Inflate leading so the ruby/emphasis interaction must account for it. */
    span{line-height:3;text-emphasis-style:dot;text-emphasis-position:over;}
    rt{font-size:20px;line-height:1;}
  </style></head><body><ruby><span>A</span><rt>B</rt></ruby></body></html>"#;

  let font_config = FontConfig::default()
    .with_system_fonts(false)
    .with_bundled_fonts(true);
  let mut renderer = FastRender::builder()
    .font_sources(font_config)
    .build()
    .expect("renderer");
  let options = RenderOptions::new().with_viewport(200, 200);
  let mut artifacts = RenderArtifacts::new(RenderArtifactRequest {
    fragment_tree: true,
    display_list: true,
    ..Default::default()
  });

  let _pixmap = renderer
    .render_html_with_options_and_artifacts(html, options, &mut artifacts)
    .expect("render");

  let tree = artifacts.fragment_tree.take().expect("fragment tree captured");
  let list = artifacts.display_list.take().expect("display list captured");

  let (anno_min_y, anno_max_y) = text_y_span(&tree.root, "B").expect("ruby annotation fragment");
  let (base_min_y, base_max_y) = text_y_span(&tree.root, "A").expect("base text fragment span");
  let base_offset = text_emphasis_offset(&tree.root, "A").expect("base text fragment");
  assert!(
    base_offset.over > 0.0,
    "expected non-zero emphasis offset for ruby interaction, got {base_offset:?}"
  );

  let emphasis = list
    .items()
    .iter()
    .find_map(|item| match item {
      DisplayItem::Text(text) => text.emphasis.as_ref(),
      _ => None,
    })
    .expect("expected emphasis marks in display list");

  assert_eq!(
    emphasis.position,
    TextEmphasisPosition::Over,
    "text-emphasis-position should not be flipped due to ruby"
  );
  assert!(
    !emphasis.marks.is_empty(),
    "expected at least one emphasis mark"
  );

  for mark in &emphasis.marks {
    assert!(
      mark.center.y < anno_min_y - 0.01,
      "emphasis mark should be outside ruby annotation bounds (mark_y={}, anno=[{}, {}], base=[{}, {}], base_offset={:?})",
      mark.center.y,
      anno_min_y,
      anno_max_y,
      base_min_y,
      base_max_y,
      base_offset
    );
  }
}
