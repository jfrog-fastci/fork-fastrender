use crate::text::font_db::FontMetrics;
use crate::tree::fragment_tree::FragmentContent;
use crate::{FastRender, FastRenderConfig, FontConfig, FontContext};

const ROBOTO_FLEX: &[u8] =
  include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fonts/RobotoFlex-VF.ttf"));

fn find_text_fragment<'a>(
  fragment: &'a crate::FragmentNode,
  needle: &str,
) -> Option<&'a crate::FragmentNode> {
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

fn normal_line_height_from_runs(font_ctx: &FontContext, runs: &[crate::ShapedRun]) -> f32 {
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

#[test]
fn bundled_sans_serif_normal_line_height_is_reasonable() {
  let font_ctx = FontContext::with_config(FontConfig::bundled_only());
  let font = font_ctx.get_sans_serif().expect("bundled sans-serif font");
  assert_eq!(font.family, "Roboto Flex");
  let face = ttf_parser::Face::parse(font.data.as_slice(), font.index)
    .expect("parse bundled sans-serif font");
  assert!(
    face.glyph_index('a').is_some(),
    "expected bundled sans-serif font to cover basic Latin; got {}",
    font.family
  );
  let metrics = font_ctx
    .get_scaled_metrics(&font, 16.0)
    .expect("scaled metrics");
  let line_height = metrics.line_height;

  // Our fixtures compare against Chrome. If the bundled sans-serif has unusually tall line
  // metrics, "line-height: normal" becomes much larger than Chrome's default fonts and produces
  // massive vertical drift in text-heavy pages.
  assert!(
    line_height.is_finite() && line_height > 0.0,
    "expected finite, positive line-height; got {line_height} for {}",
    font.family
  );
  assert!(
    (line_height - 18.0).abs() <= 0.2,
    "expected bundled sans-serif normal line-height to snap near Chrome output (~18px at 16px); got {line_height} for {}",
    font.family
  );
}

#[test]
fn roboto_flex_line_height_normal_truncates_like_chrome() {
  // Roboto Flex's raw typographic line height at 21px is ~24.61px. Headless Chrome truncates
  // this to 24px, so our `line-height: normal` snapping should do the same to avoid per-line
  // drift (notably on xkcd.com).
  let metrics = FontMetrics::from_data(ROBOTO_FLEX, 0).expect("Roboto Flex metrics");
  let scaled = metrics.scale(21.0);
  assert!(
    (scaled.line_height - 24.0).abs() < 0.01,
    "expected snapped line height of 24px at 21px font size; got {}",
    scaled.line_height
  );
}
