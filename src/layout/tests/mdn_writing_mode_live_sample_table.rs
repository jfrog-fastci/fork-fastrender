use crate::geometry::{Point, Rect};
use crate::style::types::WritingMode;
use crate::text::pipeline::RunRotation;
use crate::tree::fragment_tree::{FragmentContent, FragmentNode};
use crate::{FastRender, FontConfig};
use std::collections::HashMap;

fn collect_text_fragments<'a>(root: &'a FragmentNode, out: &mut Vec<&'a FragmentNode>) {
  let mut stack = vec![root];
  while let Some(node) = stack.pop() {
    if matches!(node.content, FragmentContent::Text { .. }) {
      out.push(node);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
}

fn assert_run_rotations(mode: WritingMode, rotations: &[RunRotation]) {
  assert!(
    !rotations.is_empty(),
    "expected shaped runs for {mode:?} text, but got none"
  );

  match mode {
    WritingMode::HorizontalTb => {
      assert!(
        rotations.iter().all(|rotation| *rotation == RunRotation::None),
        "expected horizontal-tb runs to have RunRotation::None, got {rotations:?}"
      );
    }
    WritingMode::VerticalRl | WritingMode::VerticalLr => {
      assert!(
        rotations.iter().any(|rotation| *rotation == RunRotation::Cw90),
        "expected vertical writing-mode runs to include at least one RunRotation::Cw90, got {rotations:?}"
      );
    }
    WritingMode::SidewaysLr => {
      assert!(
        rotations
          .iter()
          .all(|rotation| *rotation == RunRotation::Ccw90),
        "expected sideways-lr runs to all be RunRotation::Ccw90, got {rotations:?}"
      );
    }
    WritingMode::SidewaysRl => {
      assert!(
        rotations.iter().all(|rotation| *rotation == RunRotation::Cw90),
        "expected sideways-rl runs to all be RunRotation::Cw90, got {rotations:?}"
      );
    }
  }
}

#[test]
fn mdn_writing_mode_live_sample_table() {
  // Mirrors the MDN `writing-mode` article’s “Using multiple writing modes” live sample.
  //
  // This is an end-to-end regression test for:
  // - `@supports (writing-mode: sideways-lr)` evaluating true (toggling `.notice` / `.experimental`)
  // - table layout + display: table-row visibility
  // - writing-mode inheritance into text fragments
  // - shaping run rotations derived from writing-mode + default text-orientation:mixed
  const NOTICE_TEXT: &str = "MDN_WRITING_MODE_NOTICE_ROW";

  let html = format!(
    r#"<style>
table {{ font-family: "DejaVu Sans", sans-serif; font-size: 16px; line-height: 1; border-collapse: collapse; }}
td {{ padding: 0; }}

.experimental {{ display: none; }}
.notice {{ display: table-row; }}

@supports (writing-mode: sideways-lr) {{
  .experimental {{ display: table-row; }}
  .notice {{ display: none; }}
}}

.text1 td {{ writing-mode: horizontal-tb; }}
.text2 td {{ writing-mode: vertical-lr; }}
.text3 td {{ writing-mode: vertical-rl; }}
.text4 td {{ writing-mode: sideways-lr; }}
.text5 td {{ writing-mode: sideways-rl; }}
</style>
<table>
  <tr class="text1"><td>Example</td></tr>
  <tr class="text2"><td>Example</td></tr>
  <tr class="text3"><td>Example</td></tr>
  <tr class="text4 experimental"><td>Example</td></tr>
  <tr class="text5 experimental"><td>Example</td></tr>
  <tr class="notice"><td>{NOTICE_TEXT}</td></tr>
</table>"#
  );

  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer");
  let dom = renderer.parse_html(&html).expect("parse HTML");
  let fragments = renderer
    .layout_document(&dom, 800, 600)
    .expect("layout document");

  let mut text_fragments = Vec::new();
  collect_text_fragments(&fragments.root, &mut text_fragments);

  // (1) Ensure the notice row is hidden (proves @supports evaluated true).
  let notice_fragment = text_fragments.iter().find_map(|node| {
    let FragmentContent::Text { text, .. } = &node.content else {
      return None;
    };
    text.contains(NOTICE_TEXT).then_some(node)
  });
  assert!(
    notice_fragment.is_none(),
    "expected notice row to be hidden by the @supports gate, but found a fragment containing {NOTICE_TEXT:?}"
  );

  // Gather all `Example` text fragments and validate their writing-modes + rotations.
  let mut example_counts: HashMap<WritingMode, usize> = HashMap::new();

  for node in &text_fragments {
    let FragmentContent::Text { text, shaped, .. } = &node.content else {
      continue;
    };
    if text.trim() != "Example" {
      continue;
    }

    let style = node
      .style
      .as_deref()
      .expect("expected computed style on text fragment");
    let mode = style.writing_mode;

    let expected_modes = [
      WritingMode::HorizontalTb,
      WritingMode::VerticalLr,
      WritingMode::VerticalRl,
      WritingMode::SidewaysLr,
      WritingMode::SidewaysRl,
    ];
    assert!(
      expected_modes.contains(&mode),
      "unexpected writing-mode on Example text fragment: {mode:?}"
    );

    let shaped = shaped
      .as_ref()
      .expect("expected shaped runs on Example text fragment");
    let rotations: Vec<RunRotation> = shaped.iter().map(|run| run.rotation).collect();
    assert_run_rotations(mode, &rotations);

    *example_counts.entry(mode).or_insert(0) += 1;
  }

  // (2) Ensure sideways rows are present (proves `.experimental` got turned on by @supports).
  assert!(
    example_counts.get(&WritingMode::SidewaysLr).is_some(),
    "expected at least one sideways-lr Example fragment, but saw {example_counts:?}"
  );
  assert!(
    example_counts.get(&WritingMode::SidewaysRl).is_some(),
    "expected at least one sideways-rl Example fragment, but saw {example_counts:?}"
  );

  // (3) Ensure `writing-mode` propagated into all expected rows.
  for mode in [
    WritingMode::HorizontalTb,
    WritingMode::VerticalLr,
    WritingMode::VerticalRl,
    WritingMode::SidewaysLr,
    WritingMode::SidewaysRl,
  ] {
    assert!(
      example_counts.get(&mode).is_some(),
      "missing Example fragment for writing-mode {mode:?}; saw {example_counts:?}"
    );
  }
}

#[test]
fn table_cell_vertical_rl_uses_block_axis_inversion() {
  // Regression test for `writing-mode: vertical-rl` inside table cells: the block axis is
  // horizontal and flows right-to-left, so a single vertical line should be anchored to the
  // right edge of the cell (unlike `vertical-lr`, which anchors left).
  //
  // The MDN "Using multiple writing modes" fixture (`mdn_writing_mode_multiple`) exposed a bug
  // where `vertical-rl` behaved like `vertical-lr` in tables because cell fragments were being
  // unconverted twice (once in table layout and again by the parent block formatting context).
  let html = r#"<style>
table {
  font-family: "DejaVu Sans", sans-serif;
  font-size: 16px;
  line-height: 1;
  border-collapse: collapse;
  table-layout: fixed;
  width: 240px;
}
td { padding: 0; border: 0; }
.lr td { writing-mode: vertical-lr; }
.rl td { writing-mode: vertical-rl; }
</style>
<table>
  <tr class="lr"><td>Example</td></tr>
  <tr class="rl"><td>Example</td></tr>
</table>"#;

  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer");
  let dom = renderer.parse_html(html).expect("parse HTML");
  let fragments = renderer
    .layout_document(&dom, 800, 600)
    .expect("layout document");

  // Capture global (viewport-relative) bounds so we observe offsets applied by ancestor fragments
  // (e.g. the line box positioned at the right edge of the cell for vertical-rl).
  let mut vertical_lr_example: Option<(Rect, &FragmentNode)> = None;
  let mut vertical_rl_example: Option<(Rect, &FragmentNode)> = None;

  let mut stack = vec![(&fragments.root, Point::ZERO)];
  while let Some((node, offset)) = stack.pop() {
    let origin = Point::new(offset.x + node.bounds.x(), offset.y + node.bounds.y());
    if let FragmentContent::Text { text, .. } = &node.content {
      if text.trim() == "Example" {
        let style = node
          .style
          .as_deref()
          .expect("expected computed style on Example text fragment");
        let global_bounds = Rect::new(origin, node.bounds.size);
        match style.writing_mode {
          WritingMode::VerticalLr => {
            if vertical_lr_example.is_none() {
              vertical_lr_example = Some((global_bounds, node));
            }
          }
          WritingMode::VerticalRl => {
            if vertical_rl_example.is_none() {
              vertical_rl_example = Some((global_bounds, node));
            }
          }
          _ => {}
        }
      }
    }

    for child in node.children.iter().rev() {
      stack.push((child, origin));
    }
  }

  let (lr_bounds, _) = vertical_lr_example.expect("missing vertical-lr Example fragment");
  let (rl_bounds, _) = vertical_rl_example.expect("missing vertical-rl Example fragment");

  let delta_x = rl_bounds.x() - lr_bounds.x();
  assert!(
    delta_x > 50.0,
    "expected vertical-rl Example fragment to be positioned to the right of vertical-lr (delta_x={delta_x:.2}, lr_bounds={:?}, rl_bounds={:?})",
    lr_bounds,
    rl_bounds
  );
}
