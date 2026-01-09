use fastrender::tree::fragment_tree::FragmentContent;
use fastrender::{FastRender, FastRenderConfig, FontConfig, FontContext};

fn find_text_fragment<'a>(
  fragment: &'a fastrender::FragmentNode,
  needle: &str,
) -> Option<&'a fastrender::FragmentNode> {
  let mut stack = vec![fragment];
  while let Some(node) = stack.pop() {
    if let FragmentContent::Text { text, .. } = &node.content {
      if text.contains(needle) {
        return Some(node);
      }
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  None
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
fn line_height_normal_uses_fallback_font_metrics_for_text_fragments() {
  let config = FastRenderConfig::default().with_font_sources(FontConfig::bundled_only());
  let mut renderer = FastRender::with_config(config).expect("renderer");

  // Use an inline element whose primary font lacks 😀 so shaping falls back to the bundled emoji
  // face. `line-height: normal` should use the fallback font's metrics, not the first family entry.
  let html = r#"
    <html>
      <body style="margin:0">
        <div style="font-family:'DejaVu Sans'; font-size:40px">
          <span style="font-family:'DejaVu Sans','FastRender Emoji'; line-height:normal">😀</span>
        </div>
      </body>
    </html>
  "#;

  let dom = renderer.parse_html(html).expect("parse");
  let tree = renderer.layout_document(&dom, 200, 200).expect("layout");

  let emoji_frag = find_text_fragment(&tree.root, "😀").expect("emoji fragment");
  let runs = match &emoji_frag.content {
    FragmentContent::Text {
      shaped: Some(shaped),
      ..
    } => shaped.as_ref(),
    _ => panic!("expected shaped text fragment"),
  };

  let font_ctx = FontContext::with_config(FontConfig::bundled_only());
  let expected = normal_line_height_from_runs(&font_ctx, runs);
  assert!(
    expected > 0.0,
    "expected non-zero line-height from shaped runs (runs={})",
    runs.len()
  );

  let actual = emoji_frag.bounds.height();
  assert!(
    (actual - expected).abs() < 0.01,
    "expected text fragment height to match fallback font line-height (expected={expected}, actual={actual})"
  );
}
