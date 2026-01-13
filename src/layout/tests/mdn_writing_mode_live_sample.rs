use crate::tree::fragment_tree::FragmentContent;
use crate::{FastRender, FastRenderConfig, FontConfig, FontContext};

fn fragment_tree_contains_text(node: &crate::FragmentNode, needle: &str) -> bool {
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

fn line_fragments_with_text<'a>(
  fragment: &'a crate::FragmentNode,
  needle: &str,
) -> Vec<&'a crate::FragmentNode> {
  fn contains_text(node: &crate::FragmentNode, needle: &str) -> bool {
    fragment_tree_contains_text(node, needle)
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
  fragment: &'a crate::FragmentNode,
  needle: &str,
) -> &'a [crate::ShapedRun] {
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
fn mdn_using_multiple_writing_modes_live_sample_layout_regression() {
  let config = FastRenderConfig::default().with_font_sources(FontConfig::bundled_only());
  let mut renderer = FastRender::with_config(config).expect("renderer");

  // Minimalized from MDN's `writing-mode` live sample (`using_multiple_writing_modes`).
  //
  // Key integration points:
  // - table layout + per-cell `writing-mode`
  // - `@supports (writing-mode: sideways-lr)` gates `.experimental` + `.notice` rows
  // - mixed scripts in the same table to exercise fallback shaping paths
  let html = r#"
    <html>
      <head>
        <style>
          body {
            margin: 0;
            font-family: "DejaVu Sans", "Noto Sans JP", "FastRender Emoji";
            font-size: 40px;
            line-height: normal;
          }

          table {
            border-collapse: collapse;
          }

          th, td {
            border: 1px solid black;
            padding: 4px;
          }

          .experimental { display: none; }
          .notice { display: table-row; }

          @supports (writing-mode: sideways-lr) {
            .experimental { display: table-row; }
            .notice { display: none; }
          }

          .text1 td { writing-mode: horizontal-tb; }
          .text2 td { writing-mode: vertical-rl; text-orientation: upright; }
          .text3 td { writing-mode: vertical-lr; text-orientation: upright; }
          .text4 td { writing-mode: sideways-rl; }
          .text5 td { writing-mode: sideways-lr; }
        </style>
      </head>
      <body>
        <table>
          <thead>
            <tr>
              <th>Mode</th>
              <th>Sample</th>
            </tr>
          </thead>
          <tbody>
            <tr class="text1">
              <th>horizontal-tb</th>
              <td>HAB<br>HAB</td>
            </tr>
            <tr class="text2">
              <th>vertical-rl</th>
              <td>VAB<br>VAB</td>
            </tr>
            <tr class="text3">
              <th>vertical-lr</th>
              <td>日本語 😀</td>
            </tr>
            <tr class="text4 experimental">
              <th>sideways-rl</th>
              <td>SIDEWAYS-RL</td>
            </tr>
            <tr class="text5 experimental">
              <th>sideways-lr</th>
              <td>SIDEWAYS-LR</td>
            </tr>
            <tr class="notice">
              <td colspan="2">NOTICE_ROW</td>
            </tr>
          </tbody>
        </table>
      </body>
    </html>
  "#;

  let dom = renderer.parse_html(html).expect("parse");
  let tree = renderer.layout_document(&dom, 800, 600).expect("layout");

  // --- @supports gating: match Chrome baseline for this sample ---
  // Chrome supports `writing-mode: sideways-lr`, so the @supports condition is true:
  // - `.experimental` rows become visible
  // - `.notice` row is hidden
  assert!(
    !fragment_tree_contains_text(&tree.root, "NOTICE_ROW"),
    "expected `.notice` row hidden when @supports(writing-mode:sideways-lr) is true"
  );
  assert!(
    fragment_tree_contains_text(&tree.root, "SIDEWAYS-LR"),
    "expected `.experimental` sideways-lr row to be visible when @supports(writing-mode:sideways-lr) is true"
  );
  assert!(
    fragment_tree_contains_text(&tree.root, "SIDEWAYS-RL"),
    "expected `.experimental` sideways-rl row to be visible when @supports(writing-mode:sideways-lr) is true"
  );

  // --- Writing-mode axis mapping regression (table-cell IFC) ---
  let horizontal_lines = line_fragments_with_text(&tree.root, "HAB");
  let vertical_lines = line_fragments_with_text(&tree.root, "VAB");
  assert_eq!(
    horizontal_lines.len(),
    2,
    "expected exactly two line fragments in the horizontal-tb table cell"
  );
  assert_eq!(
    vertical_lines.len(),
    2,
    "expected exactly two line fragments in the vertical-rl table cell"
  );

  let font_ctx = FontContext::with_config(FontConfig::bundled_only());
  let expected_horizontal = normal_line_height_from_runs(&font_ctx, first_text_runs(&tree.root, "HAB"));
  let expected_vertical = normal_line_height_from_runs(&font_ctx, first_text_runs(&tree.root, "VAB"));

  assert!(
    expected_horizontal > 0.0 && expected_vertical > 0.0,
    "expected non-zero line-height from shaped runs"
  );

  for line in &horizontal_lines {
    let actual = line.bounds.height();
    assert!(
      (actual - expected_horizontal).abs() < 0.01,
      "expected horizontal line fragment height to equal line-height (expected={expected_horizontal}, actual={actual})"
    );
  }

  for line in &vertical_lines {
    let actual = line.bounds.width();
    assert!(
      (actual - expected_vertical).abs() < 0.01,
      "expected vertical-rl line fragment width to equal line-height (expected={expected_vertical}, actual={actual})"
    );
  }

  let horizontal_delta = (horizontal_lines[0].bounds.y() - horizontal_lines[1].bounds.y()).abs();
  assert!(
    (horizontal_delta - expected_horizontal).abs() < 0.5,
    "expected horizontal line stacking delta to match line-height (expected={expected_horizontal}, actual={horizontal_delta})"
  );

  let vertical_delta = (vertical_lines[0].bounds.x() - vertical_lines[1].bounds.x()).abs();
  assert!(
    (vertical_delta - expected_vertical).abs() < 0.5,
    "expected vertical-rl line stacking delta to match line-height (expected={expected_vertical}, actual={vertical_delta})"
  );
}
