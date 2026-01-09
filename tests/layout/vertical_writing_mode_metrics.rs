use fastrender::tree::fragment_tree::FragmentContent;
use fastrender::{FastRender, FastRenderConfig, FontConfig, FontContext};

fn line_fragments_with_text<'a>(
  fragment: &'a fastrender::FragmentNode,
  needle: &str,
) -> Vec<&'a fastrender::FragmentNode> {
  fn contains_text(node: &fastrender::FragmentNode, needle: &str) -> bool {
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
      if let FragmentContent::Text { text, .. } = &n.content {
        if text.contains(needle) {
          return true;
        }
      }
      for child in n.children.iter().rev() {
        stack.push(child);
      }
    }
    false
  }

  let mut out = Vec::new();
  let mut stack = vec![fragment];
  while let Some(node) = stack.pop() {
    if matches!(node.content, FragmentContent::Line { .. }) && contains_text(node, needle) {
      out.push(node);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  out
}

fn first_text_runs<'a>(
  fragment: &'a fastrender::FragmentNode,
  needle: &str,
) -> &'a [fastrender::ShapedRun] {
  let mut stack = vec![fragment];
  while let Some(node) = stack.pop() {
    if let FragmentContent::Text { text, shaped, .. } = &node.content {
      if text.contains(needle) {
        return shaped.as_ref().expect("shaped runs").as_ref();
      }
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  panic!("missing text fragment containing {needle:?}");
}

fn normal_line_height_from_runs(font_ctx: &FontContext, runs: &[fastrender::ShapedRun]) -> f32 {
  let mut max_line_height = 0.0_f32;
  for run in runs {
    if let Some(metrics) =
      font_ctx.get_scaled_metrics_with_variations(run.font.as_ref(), run.font_size, &run.variations)
    {
      max_line_height = max_line_height.max(metrics.line_height);
    }
  }
  max_line_height
}

#[test]
fn vertical_rl_line_boxes_use_line_height_on_block_axis() {
  let config = FastRenderConfig::default().with_font_sources(FontConfig::bundled_only());
  let mut renderer = FastRender::with_config(config).expect("renderer");

  let html = r#"
    <html>
      <body style="margin:0">
        <div style="writing-mode:vertical-rl; font-family:'DejaVu Sans','FastRender Emoji'; font-size:40px; line-height:normal">
          😀<br>😀
        </div>
      </body>
    </html>
  "#;

  let dom = renderer.parse_html(html).expect("parse");
  let tree = renderer.layout_document(&dom, 200, 200).expect("layout");

  let lines = line_fragments_with_text(&tree.root, "😀");
  assert_eq!(lines.len(), 2, "expected exactly two line fragments");

  let font_ctx = FontContext::with_config(FontConfig::bundled_only());
  let expected = normal_line_height_from_runs(&font_ctx, first_text_runs(&tree.root, "😀"));
  assert!(
    expected > 0.0,
    "expected non-zero line-height from shaped runs"
  );

  for line in &lines {
    let actual = line.bounds.width();
    assert!(
      (actual - expected).abs() < 0.01,
      "expected vertical-rl line fragment width to equal line-height (expected={expected}, actual={actual})"
    );
  }

  let delta = (lines[0].bounds.x() - lines[1].bounds.x()).abs();
  assert!(
    (delta - expected).abs() < 0.5,
    "expected vertical-rl line stacking delta to match line-height (expected={expected}, actual={delta})"
  );
}

#[test]
fn sideways_rl_line_boxes_use_line_height_on_block_axis() {
  let config = FastRenderConfig::default().with_font_sources(FontConfig::bundled_only());
  let mut renderer = FastRender::with_config(config).expect("renderer");

  let html = r#"
    <html>
      <body style="margin:0">
        <div style="writing-mode:sideways-rl; font-family:'DejaVu Sans'; font-size:40px; line-height:normal">
          AB<br>AB
        </div>
      </body>
    </html>
  "#;

  let dom = renderer.parse_html(html).expect("parse");
  let tree = renderer.layout_document(&dom, 200, 200).expect("layout");

  let lines = line_fragments_with_text(&tree.root, "AB");
  assert_eq!(lines.len(), 2, "expected exactly two line fragments");

  let font_ctx = FontContext::with_config(FontConfig::bundled_only());
  let expected = normal_line_height_from_runs(&font_ctx, first_text_runs(&tree.root, "AB"));
  assert!(
    expected > 0.0,
    "expected non-zero line-height from shaped runs"
  );

  for line in &lines {
    let actual = line.bounds.width();
    assert!(
      (actual - expected).abs() < 0.01,
      "expected sideways-rl line fragment width to equal line-height (expected={expected}, actual={actual})"
    );
  }

  let delta = (lines[0].bounds.x() - lines[1].bounds.x()).abs();
  assert!(
    (delta - expected).abs() < 0.5,
    "expected sideways-rl line stacking delta to match line-height (expected={expected}, actual={delta})"
  );
}
