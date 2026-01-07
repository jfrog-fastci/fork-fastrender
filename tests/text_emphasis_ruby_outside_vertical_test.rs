use fastrender::paint::display_list::DisplayItem;
use fastrender::style::types::TextEmphasisPosition;
use fastrender::text::font_db::FontConfig;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode, TextEmphasisOffset};
use fastrender::{FastRender, Point, RenderArtifactRequest, RenderArtifacts, RenderOptions};

fn text_x_span(root: &FragmentNode, needle: &str) -> Option<(f32, f32)> {
  let mut min_x = f32::INFINITY;
  let mut max_x = f32::NEG_INFINITY;

  let mut stack: Vec<(&FragmentNode, Point)> = vec![(root, Point::ZERO)];
  while let Some((node, offset)) = stack.pop() {
    let abs_x = offset.x + node.bounds.x();
    let abs_y = offset.y + node.bounds.y();

    if let FragmentContent::Text { text, .. } = &node.content {
      if text.as_ref() == needle {
        min_x = min_x.min(abs_x);
        max_x = max_x.max(abs_x + node.bounds.width());
      }
    }

    let child_offset = Point::new(abs_x, abs_y);
    for child in node.children.iter().rev() {
      stack.push((child, child_offset));
    }
  }

  if min_x.is_finite() {
    Some((min_x, max_x))
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
fn text_emphasis_marks_render_outside_ruby_annotations_in_vertical_writing() {
  let html = r#"<!doctype html><html><head><style>
    body{margin:0;font-size:40px;writing-mode:vertical-rl;}
    ruby{ruby-position:over;}
    /* Inflate leading so the ruby/emphasis interaction must account for it. */
    span{line-height:3;text-emphasis-style:dot;text-emphasis-position:over right;}
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

  let (anno_min_x, anno_max_x) = text_x_span(&tree.root, "B").expect("ruby annotation fragment");
  let (base_min_x, base_max_x) = text_x_span(&tree.root, "A").expect("base text fragment span");
  let base_offset = text_emphasis_offset(&tree.root, "A").expect("base text fragment");

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
    TextEmphasisPosition::OverRight,
    "text-emphasis-position should not be flipped due to ruby"
  );
  assert!(
    emphasis.inline_vertical,
    "expected vertical emphasis marks in vertical writing"
  );
  assert!(
    !emphasis.marks.is_empty(),
    "expected at least one emphasis mark"
  );

  // Determine which side the ruby annotation sits on relative to the base glyph, then assert the
  // emphasis marks are outside the ruby on that same side.
  let ruby_on_right = anno_min_x >= base_max_x - 0.01;
  let ruby_on_left = anno_max_x <= base_min_x + 0.01;
  assert!(
    ruby_on_right ^ ruby_on_left,
    "expected ruby annotation to be on exactly one side of the base text (base=[{}, {}], anno=[{}, {}])",
    base_min_x,
    base_max_x,
    anno_min_x,
    anno_max_x
  );

  for mark in &emphasis.marks {
    let mark_on_right = mark.center.x > (base_min_x + base_max_x) * 0.5;
    if ruby_on_right {
      assert!(
        mark_on_right,
        "expected emphasis marks on the same side as ruby (right)"
      );
      assert!(
        mark.center.x > anno_max_x + 0.01,
        "emphasis mark should be outside ruby annotation bounds (mark_x={}, anno=[{}, {}], base=[{}, {}], base_offset={:?})",
        mark.center.x,
        anno_min_x,
        anno_max_x,
        base_min_x,
        base_max_x,
        base_offset
      );
      assert!(
        base_offset.over > 0.0,
        "expected non-zero emphasis offset for ruby interaction, got {base_offset:?}"
      );
    } else {
      assert!(
        !mark_on_right,
        "expected emphasis marks on the same side as ruby (left)"
      );
      assert!(
        mark.center.x < anno_min_x - 0.01,
        "emphasis mark should be outside ruby annotation bounds (mark_x={}, anno=[{}, {}], base=[{}, {}], base_offset={:?})",
        mark.center.x,
        anno_min_x,
        anno_max_x,
        base_min_x,
        base_max_x,
        base_offset
      );
      assert!(
        base_offset.under > 0.0,
        "expected non-zero emphasis offset for ruby interaction, got {base_offset:?}"
      );
    }
  }
}

