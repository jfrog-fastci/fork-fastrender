use crate::style::types::WritingMode;
use crate::text::pipeline::RunRotation;
use crate::tree::fragment_tree::FragmentContent;
use crate::{FastRender, FastRenderConfig, FontConfig, FontContext};

fn find_text_fragment<'a>(
  fragment: &'a crate::FragmentNode,
  needle: &str,
) -> &'a crate::FragmentNode {
  let mut stack = vec![fragment];
  while let Some(node) = stack.pop() {
    if let FragmentContent::Text { text, .. } = &node.content {
      if text.contains(needle) {
        return node;
      }
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  panic!("missing text fragment containing {needle:?}");
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

fn assert_sideways_text_layout(mode: WritingMode, expected_rotation: RunRotation) {
  let config = FastRenderConfig::default().with_font_sources(FontConfig::bundled_only());
  let mut renderer = FastRender::with_config(config).expect("renderer");

  let mode_css = match mode {
    WritingMode::SidewaysLr => "sideways-lr",
    WritingMode::SidewaysRl => "sideways-rl",
    other => panic!("unexpected writing mode for sideways test: {other:?}"),
  };

  let html = format!(
    r#"
      <html>
        <body style="margin:0">
          <div style="writing-mode:{mode_css}; font-size:20px; font-family:'DejaVu Sans', sans-serif;">Ab本</div>
        </body>
      </html>
    "#
  );

  let dom = renderer.parse_html(&html).expect("parse");
  let tree = renderer.layout_document(&dom, 200, 200).expect("layout");

  let text_fragment = find_text_fragment(&tree.root, "Ab本");
  let style = text_fragment.style.as_ref().expect("text fragment style");
  assert_eq!(
    style.writing_mode, mode,
    "text fragment should preserve writing-mode for sideways layout"
  );

  let runs = match &text_fragment.content {
    FragmentContent::Text {
      shaped: Some(shaped),
      ..
    } => shaped.as_ref(),
    _ => panic!("expected shaped text fragment"),
  };
  assert!(
    runs.len() >= 2,
    "expected at least two shaped runs (Latin + CJK), got {}",
    runs.len()
  );

  for (idx, run) in runs.iter().enumerate() {
    assert_eq!(
      run.rotation, expected_rotation,
      "run[{idx}] rotation mismatch for {mode:?}"
    );
    assert!(
      !run.glyphs.is_empty(),
      "run[{idx}] should contain glyphs for {mode:?}"
    );

    // Sideways modes rotate at paint time but still shape using horizontal advances.
    let max_abs_y_advance = run
      .glyphs
      .iter()
      .map(|g| g.y_advance.abs())
      .fold(0.0_f32, f32::max);
    assert!(
      max_abs_y_advance < 0.01,
      "run[{idx}] expected horizontal shaping (y_advance≈0), got max |y_advance|={max_abs_y_advance}"
    );

    let sum_abs_x_advance: f32 = run.glyphs.iter().map(|g| g.x_advance.abs()).sum();
    assert!(
      sum_abs_x_advance > 0.1,
      "run[{idx}] expected non-zero x advances for horizontal shaping, got sum |x_advance|={sum_abs_x_advance}"
    );
  }

  // Bounds should reflect the rotated writing mode: inline-size maps to the vertical axis.
  let expected_inline_advance: f32 = runs.iter().map(|r| r.advance.abs()).sum();
  assert!(
    expected_inline_advance > 0.0,
    "expected non-zero inline advance from shaped runs"
  );

  let font_ctx = FontContext::with_config(FontConfig::bundled_only());
  let expected_line_height = normal_line_height_from_runs(&font_ctx, runs);
  assert!(
    expected_line_height > 0.0,
    "expected non-zero line-height from shaped runs"
  );

  let phys_width = text_fragment.bounds.width();
  let phys_height = text_fragment.bounds.height();
  assert!(
    (phys_width - expected_line_height).abs() < 0.5,
    "expected sideways text fragment width to reflect line-height (expected≈{expected_line_height}, got {phys_width})"
  );
  assert!(
    (phys_height - expected_inline_advance).abs() < 0.5,
    "expected sideways text fragment height to reflect inline advance (expected≈{expected_inline_advance}, got {phys_height})"
  );
  assert!(
    phys_height > phys_width,
    "expected inline-size to map to the vertical axis under {mode:?} (height={phys_height}, width={phys_width})"
  );
}

#[test]
fn sideways_writing_mode_layout() {
  assert_sideways_text_layout(WritingMode::SidewaysLr, RunRotation::Ccw90);
  assert_sideways_text_layout(WritingMode::SidewaysRl, RunRotation::Cw90);
}

