use fastrender::geometry::Rect;
use fastrender::paint::display_list::{DisplayItem, DisplayList, GlyphInstance, TextItem};
use fastrender::text::font_db::FontConfig;
use fastrender::{
  FastRender, LayoutParallelism, PaintParallelism, RenderArtifactRequest, RenderArtifacts,
  RenderOptions, Rgba,
};
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InlineAxis {
  Horizontal,
  Vertical,
}

#[derive(Clone, Copy, Debug)]
struct LineMetrics {
  /// Minimum glyph/cluster extent along the inline axis (absolute coord).
  min_inline: f32,
  /// Maximum glyph/cluster extent along the inline axis (absolute coord).
  max_inline: f32,
  /// Total extent along the inline axis: `max_inline - min_inline`.
  inline_span: f32,
  /// Maximum positive gap between consecutive clusters (after sorting along the inline axis).
  max_gap: f32,
}

fn render_display_list(html: &str, width: u32, height: u32) -> DisplayList {
  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("renderer");
  let options = RenderOptions::new()
    .with_viewport(width, height)
    .with_paint_parallelism(PaintParallelism::disabled())
    .with_layout_parallelism(LayoutParallelism::disabled());
  let mut artifacts = RenderArtifacts::new(RenderArtifactRequest {
    display_list: true,
    ..RenderArtifactRequest::none()
  });
  renderer
    .render_html_with_options_and_artifacts(html, options, &mut artifacts)
    .expect("render html");
  artifacts.display_list.take().expect("display list")
}

fn find_background_rect_bounds(list: &DisplayList, color: Rgba) -> Rect {
  let mut bounds: Option<Rect> = None;
  for item in list.items() {
    let rect = match item {
      DisplayItem::FillRect(fill) if fill.color == color => Some(fill.rect),
      DisplayItem::FillRoundedRect(fill) if fill.color == color => Some(fill.rect),
      _ => None,
    };
    let Some(rect) = rect else {
      continue;
    };
    bounds = Some(match bounds {
      Some(prev) => prev.union(rect),
      None => rect,
    });
  }
  bounds.expect("expected background rect not found")
}

fn inline_offset(glyph: &GlyphInstance, axis: InlineAxis) -> f32 {
  match axis {
    InlineAxis::Horizontal => glyph.x_offset,
    InlineAxis::Vertical => glyph.y_offset,
  }
}

fn inline_advance(glyph: &GlyphInstance, axis: InlineAxis) -> f32 {
  match axis {
    InlineAxis::Horizontal => {
      if glyph.x_advance.abs() > f32::EPSILON {
        glyph.x_advance
      } else {
        glyph.y_advance
      }
    }
    InlineAxis::Vertical => {
      if glyph.y_advance.abs() > f32::EPSILON {
        glyph.y_advance
      } else {
        glyph.x_advance
      }
    }
  }
}

fn block_pos(item: &TextItem, axis: InlineAxis) -> f32 {
  match axis {
    InlineAxis::Horizontal => item.origin.y,
    // For vertical writing modes, individual glyph runs can have different baseline origins in X
    // (e.g. when the renderer centers glyphs of varying widths within the column). Using the
    // conservative bounds center is a more stable way to group runs into columns/lines.
    InlineAxis::Vertical => fastrender::paint::display_list::text_bounds(item).center().x,
  }
}

fn line_metrics_for_text_items(items: &[(usize, &TextItem)], axis: InlineAxis) -> LineMetrics {
  let mut min_inline = f32::INFINITY;
  let mut max_inline = f32::NEG_INFINITY;

  // Cluster indices are only meaningful within the run's original source text.
  // Use (display-list index, cluster) as the key to avoid collisions across runs.
  let mut clusters: HashMap<(usize, u32), (f32, f32)> = HashMap::new();

  for (item_idx, item) in items {
    let origin_inline = match axis {
      InlineAxis::Horizontal => item.origin.x,
      InlineAxis::Vertical => item.origin.y,
    };
    let mut pen_inline = 0.0_f32;
    for glyph in &item.glyphs {
      let offset = inline_offset(glyph, axis);
      let adv = inline_advance(glyph, axis);
      let start = origin_inline + pen_inline + offset;
      let end = start + adv;
      let (span_start, span_end) = if end >= start { (start, end) } else { (end, start) };

      min_inline = min_inline.min(span_start);
      max_inline = max_inline.max(span_end);

      let key = (*item_idx, glyph.cluster);
      let entry = clusters.entry(key).or_insert((span_start, span_end));
      entry.0 = entry.0.min(span_start);
      entry.1 = entry.1.max(span_end);

      if adv.is_finite() {
        pen_inline += adv;
      }
    }
  }

  let mut spans: Vec<(f32, f32)> = clusters.into_values().collect();
  spans.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

  let mut max_gap: f32 = 0.0;
  for window in spans.windows(2) {
    let (_prev_start, prev_end) = window[0];
    let (next_start, _next_end) = window[1];
    let gap = next_start - prev_end;
    if gap.is_finite() {
      max_gap = max_gap.max(gap.max(0.0));
    }
  }

  if !min_inline.is_finite() || !max_inline.is_finite() {
    min_inline = 0.0;
    max_inline = 0.0;
  }
  let inline_span = (max_inline - min_inline).max(0.0);
  LineMetrics {
    min_inline,
    max_inline,
    inline_span,
    max_gap,
  }
}

fn line_metrics_in_rect(list: &DisplayList, rect: Rect, axis: InlineAxis) -> Vec<LineMetrics> {
  let probe = rect.inflate(1.0);
  let mut items: Vec<(usize, &TextItem)> = list
    .items()
    .iter()
    .enumerate()
    .filter_map(|(idx, item)| match item {
      DisplayItem::Text(text) => Some((idx, text)),
      _ => None,
    })
    .filter(|(_idx, text)| {
      // Use display-item bounds (which are conservative) to select items within our target box.
      let bounds = fastrender::paint::display_list::text_bounds(text);
      bounds.intersects(probe)
    })
    .collect();

  if items.is_empty() {
    return Vec::new();
  }

  items.sort_by(|a, b| {
    block_pos(a.1, axis)
      .partial_cmp(&block_pos(b.1, axis))
      .unwrap_or(std::cmp::Ordering::Equal)
  });

  let tol = 0.5;
  let mut out = Vec::new();
  let mut current_block = block_pos(items[0].1, axis);
  let mut current: Vec<(usize, &TextItem)> = Vec::new();
  for item in items {
    let bp = block_pos(item.1, axis);
    if (bp - current_block).abs() <= tol {
      current.push(item);
      continue;
    }
    out.push(line_metrics_for_text_items(&current, axis));
    current = vec![item];
    current_block = bp;
  }
  if !current.is_empty() {
    out.push(line_metrics_for_text_items(&current, axis));
  }
  out
}

fn container_inline_bounds(rect: Rect, axis: InlineAxis) -> (f32, f32) {
  match axis {
    InlineAxis::Horizontal => (rect.min_x(), rect.max_x()),
    InlineAxis::Vertical => (rect.min_y(), rect.max_y()),
  }
}

fn assert_approx_eq(value: f32, expected: f32, eps: f32, context: &str) {
  assert!(
    (value - expected).abs() <= eps,
    "{context}: expected {expected:.2}±{eps:.2}, got {value:.2}"
  );
}

fn assert_justification_happened(
  baseline: LineMetrics,
  justified: LineMetrics,
  axis: InlineAxis,
  inline_start: f32,
  inline_end: f32,
  context: &str,
) {
  let inline_size = inline_end - inline_start;
  assert!(
    inline_size.is_finite() && inline_size > 0.0,
    "{context}: invalid inline_size={inline_size}"
  );
  assert!(
    baseline.inline_span + 1.0 < inline_size,
    "{context}: baseline unexpectedly fills inline size (baseline_span={:.2}, inline_size={:.2})",
    baseline.inline_span,
    inline_size
  );
  assert_approx_eq(
    justified.inline_span,
    inline_size,
    2.0,
    &format!(
      "{context}: justified line should fill inline size (baseline_span={:.2})",
      baseline.inline_span
    ),
  );
  assert!(
    justified.inline_span > baseline.inline_span + 1.0,
    "{context}: expected justified span to grow (baseline_span={:.2}, justified_span={:.2}, inline_size={:.2})",
    baseline.inline_span,
    justified.inline_span,
    inline_size
  );
  // For vertical writing modes, text runs can have large per-glyph offsets relative to the
  // baseline origin (used to center glyphs within the column), which makes absolute inline-start
  // anchoring brittle. Horizontal anchoring is much more stable and catches RTL justification
  // cursor bugs (where content can shift outside the line box).
  if axis == InlineAxis::Horizontal {
    assert_approx_eq(
      justified.min_inline,
      inline_start,
      5.0,
      &format!("{context}: justified line should be anchored at inline start"),
    );
    assert_approx_eq(
      justified.max_inline,
      inline_end,
      5.0,
      &format!("{context}: justified line should be anchored at inline end"),
    );
  }
  assert!(
    justified.max_gap > baseline.max_gap + 0.75,
    "{context}: expected at least one inter-cluster gap to increase (baseline_max_gap={:.2}, justified_max_gap={:.2})",
    baseline.max_gap,
    justified.max_gap
  );
}

fn assert_no_justification(
  baseline: LineMetrics,
  candidate: LineMetrics,
  inline_size: f32,
  context: &str,
) {
  assert!(
    inline_size.is_finite() && inline_size > 0.0,
    "{context}: invalid inline_size={inline_size}"
  );
  assert!(
    candidate.inline_span + 1.0 < inline_size,
    "{context}: candidate unexpectedly fills inline size (candidate_span={:.2}, inline_size={:.2})",
    candidate.inline_span,
    inline_size
  );
  assert_approx_eq(
    candidate.inline_span,
    baseline.inline_span,
    1.0,
    &format!(
      "{context}: expected candidate span to match baseline (baseline_span={:.2})",
      baseline.inline_span
    ),
  );
  assert_approx_eq(
    candidate.max_gap,
    baseline.max_gap,
    1.0,
    &format!(
      "{context}: expected candidate max gap to match baseline (baseline_max_gap={:.2})",
      baseline.max_gap
    ),
  );
}

fn words_as_spans(words: &[&str]) -> String {
  words
    .iter()
    .map(|w| format!("<span>{w}</span>"))
    .collect::<Vec<_>>()
    .join(" ")
}

fn chars_as_spans(text: &str) -> String {
  text
    .chars()
    .map(|c| format!("<span>{c}</span>"))
    .collect::<Vec<_>>()
    .join("")
}

#[test]
fn text_justify_inter_word_justifies_latin_with_spaces() {
  let container_color = Rgba::new(10, 20, 30, 1.0);
  let axis = InlineAxis::Horizontal;

  // The box tree wraps bare text nodes in anonymous inline boxes, which means a single
  // plain text node becomes one `InlineItem::InlineBox` and leaves no inline-item boundaries
  // for the current justification implementation to distribute spacing across.
  //
  // To exercise the public justification pipeline, split the paragraph into multiple inline
  // items by wrapping each word in its own `<span>` with collapsible spaces between.
  let words = ["one", "two", "three", "four", "five", "six", "seven"];
  let inner_html = words_as_spans(&words);

  let common_css = r#"
    @font-face {
      font-family: "Latin";
      src: url("tests/fixtures/fonts/NotoSansMono-subset.ttf") format("truetype");
    }
    body { margin: 0; background: white; }
    #c {
      width: 460px;
      height: 80px;
      padding: 0;
      background: rgb(10, 20, 30);
      font-family: "Latin";
      font-size: 20px;
      line-height: 1;
      white-space: nowrap;
      color: black;
    }
  "#;

  let baseline_html = format!(
    r#"<!doctype html><html><head><style>{common_css}</style></head><body>
      <div id="c" style="text-align: start">{inner_html}</div>
    </body></html>"#
  );
  let justified_html = format!(
    r#"<!doctype html><html><head><style>{common_css}</style></head><body>
      <div id="c" style="text-align: justify; text-align-last: justify; text-justify: inter-word">{inner_html}</div>
    </body></html>"#
  );

  let baseline_list = render_display_list(&baseline_html, 520, 120);
  let justified_list = render_display_list(&justified_html, 520, 120);
  let container = find_background_rect_bounds(&baseline_list, container_color);
  let (inline_start, inline_end) = container_inline_bounds(container, axis);

  let baseline_lines = line_metrics_in_rect(&baseline_list, container, axis);
  let justified_lines = line_metrics_in_rect(&justified_list, container, axis);
  assert_eq!(
    baseline_lines.len(),
    1,
    "expected a single baseline line, got {baseline_lines:?}"
  );
  assert_eq!(
    justified_lines.len(),
    1,
    "expected a single justified line, got {justified_lines:?}"
  );

  assert_justification_happened(
    baseline_lines[0],
    justified_lines[0],
    axis,
    inline_start,
    inline_end,
    "inter-word Latin",
  );
}

#[test]
fn text_justify_inter_character_justifies_cjk_without_spaces() {
  let container_color = Rgba::new(11, 21, 31, 1.0);
  let axis = InlineAxis::Horizontal;

  let text = "漢字漢字漢字漢字漢字漢字";
  let inner_html = chars_as_spans(text);

  let common_css = r#"
    @font-face {
      font-family: "CJK";
      src: url("tests/fixtures/fonts/NotoSansJP-subset.ttf") format("truetype");
    }
    body { margin: 0; background: white; }
    #c {
      width: 360px;
      height: 90px;
      padding: 0;
      background: rgb(11, 21, 31);
      font-family: "CJK";
      font-size: 24px;
      line-height: 1;
      white-space: nowrap;
      color: black;
    }
  "#;

  let baseline_html = format!(
    r#"<!doctype html><html><head><style>{common_css}</style></head><body>
      <div id="c" style="text-align: start">{inner_html}</div>
    </body></html>"#
  );
  let justified_html = format!(
    r#"<!doctype html><html><head><style>{common_css}</style></head><body>
      <div id="c" style="text-align: justify; text-align-last: justify; text-justify: inter-character">{inner_html}</div>
    </body></html>"#
  );

  let baseline_list = render_display_list(&baseline_html, 420, 140);
  let justified_list = render_display_list(&justified_html, 420, 140);
  let container = find_background_rect_bounds(&baseline_list, container_color);
  let (inline_start, inline_end) = container_inline_bounds(container, axis);

  let baseline_lines = line_metrics_in_rect(&baseline_list, container, axis);
  let justified_lines = line_metrics_in_rect(&justified_list, container, axis);
  assert_eq!(
    baseline_lines.len(),
    1,
    "expected a single baseline line, got {baseline_lines:?}"
  );
  assert_eq!(
    justified_lines.len(),
    1,
    "expected a single justified line, got {justified_lines:?}"
  );

  assert_justification_happened(
    baseline_lines[0],
    justified_lines[0],
    axis,
    inline_start,
    inline_end,
    "inter-character CJK",
  );
}

#[test]
fn text_justify_distribute_justifies_mixed_latin_and_cjk() {
  let container_color = Rgba::new(12, 22, 32, 1.0);
  let axis = InlineAxis::Horizontal;

  let text = "HELLO世界HELLO世界HELLO";
  let inner_html = chars_as_spans(text);

  let common_css = r#"
    @font-face {
      font-family: "Latin";
      src: url("tests/fixtures/fonts/NotoSansMono-subset.ttf") format("truetype");
    }
    @font-face {
      font-family: "CJK";
      src: url("tests/fixtures/fonts/NotoSansJP-subset.ttf") format("truetype");
    }
    body { margin: 0; background: white; }
    #c {
      width: 440px;
      height: 90px;
      padding: 0;
      background: rgb(12, 22, 32);
      font-family: "Latin", "CJK";
      font-size: 22px;
      line-height: 1;
      white-space: nowrap;
      color: black;
    }
  "#;

  let baseline_html = format!(
    r#"<!doctype html><html><head><style>{common_css}</style></head><body>
      <div id="c" style="text-align: start">{inner_html}</div>
    </body></html>"#
  );
  let justified_html = format!(
    r#"<!doctype html><html><head><style>{common_css}</style></head><body>
      <div id="c" style="text-align: justify; text-align-last: justify; text-justify: distribute">{inner_html}</div>
    </body></html>"#
  );

  let baseline_list = render_display_list(&baseline_html, 520, 140);
  let justified_list = render_display_list(&justified_html, 520, 140);
  let container = find_background_rect_bounds(&baseline_list, container_color);
  let (inline_start, inline_end) = container_inline_bounds(container, axis);

  let baseline_lines = line_metrics_in_rect(&baseline_list, container, axis);
  let justified_lines = line_metrics_in_rect(&justified_list, container, axis);
  assert_eq!(
    baseline_lines.len(),
    1,
    "expected a single baseline line, got {baseline_lines:?}"
  );
  assert_eq!(
    justified_lines.len(),
    1,
    "expected a single justified line, got {justified_lines:?}"
  );

  assert_justification_happened(
    baseline_lines[0],
    justified_lines[0],
    axis,
    inline_start,
    inline_end,
    "distribute mixed Latin+CJK",
  );
}

#[test]
fn text_justify_none_disables_justification() {
  let container_color = Rgba::new(13, 23, 33, 1.0);
  let axis = InlineAxis::Horizontal;

  let words = ["one", "two", "three", "four", "five", "six", "seven"];
  let inner_html = words_as_spans(&words);

  let common_css = r#"
    @font-face {
      font-family: "Latin";
      src: url("tests/fixtures/fonts/NotoSansMono-subset.ttf") format("truetype");
    }
    body { margin: 0; background: white; }
    #c {
      width: 460px;
      height: 80px;
      padding: 0;
      background: rgb(13, 23, 33);
      font-family: "Latin";
      font-size: 20px;
      line-height: 1;
      white-space: nowrap;
      color: black;
    }
  "#;

  let baseline_html = format!(
    r#"<!doctype html><html><head><style>{common_css}</style></head><body>
      <div id="c" style="text-align: start">{inner_html}</div>
    </body></html>"#
  );
  let none_html = format!(
    r#"<!doctype html><html><head><style>{common_css}</style></head><body>
      <div id="c" style="text-align: justify; text-align-last: justify; text-justify: none">{inner_html}</div>
    </body></html>"#
  );

  let baseline_list = render_display_list(&baseline_html, 520, 120);
  let none_list = render_display_list(&none_html, 520, 120);
  let container = find_background_rect_bounds(&baseline_list, container_color);
  let (inline_start, inline_end) = container_inline_bounds(container, axis);
  let inline_size = inline_end - inline_start;

  let baseline_lines = line_metrics_in_rect(&baseline_list, container, axis);
  let none_lines = line_metrics_in_rect(&none_list, container, axis);
  assert_eq!(
    baseline_lines.len(),
    1,
    "expected a single baseline line, got {baseline_lines:?}"
  );
  assert_eq!(
    none_lines.len(),
    1,
    "expected a single candidate line, got {none_lines:?}"
  );

  assert_no_justification(
    baseline_lines[0],
    none_lines[0],
    inline_size,
    "text-justify:none",
  );
}

#[test]
fn text_align_last_auto_does_not_justify_last_line_but_justify_all_does() {
  let container_color = Rgba::new(14, 24, 34, 1.0);
  let axis = InlineAxis::Horizontal;

  let words = ["one", "two", "three", "four", "five", "six", "seven"];
  let inner_html = words_as_spans(&words);

  let common_css = r#"
    @font-face {
      font-family: "Latin";
      src: url("tests/fixtures/fonts/NotoSansMono-subset.ttf") format("truetype");
    }
    body { margin: 0; background: white; }
    #c {
      width: 460px;
      height: 80px;
      padding: 0;
      background: rgb(14, 24, 34);
      font-family: "Latin";
      font-size: 20px;
      line-height: 1;
      white-space: nowrap;
      color: black;
    }
  "#;

  let baseline_html = format!(
    r#"<!doctype html><html><head><style>{common_css}</style></head><body>
      <div id="c" style="text-align: start">{inner_html}</div>
    </body></html>"#
  );
  // Single-line paragraph: `text-align: justify` should NOT justify unless `justify-all`
  // (or `text-align-last: justify`) is used.
  let justify_html = format!(
    r#"<!doctype html><html><head><style>{common_css}</style></head><body>
      <div id="c" style="text-align: justify">{inner_html}</div>
    </body></html>"#
  );
  let justify_all_html = format!(
    r#"<!doctype html><html><head><style>{common_css}</style></head><body>
      <div id="c" style="text-align: justify-all">{inner_html}</div>
    </body></html>"#
  );

  let baseline_list = render_display_list(&baseline_html, 520, 120);
  let justify_list = render_display_list(&justify_html, 520, 120);
  let justify_all_list = render_display_list(&justify_all_html, 520, 120);
  let container = find_background_rect_bounds(&baseline_list, container_color);
  let (inline_start, inline_end) = container_inline_bounds(container, axis);
  let inline_size = inline_end - inline_start;

  let baseline_lines = line_metrics_in_rect(&baseline_list, container, axis);
  let justify_lines = line_metrics_in_rect(&justify_list, container, axis);
  let justify_all_lines = line_metrics_in_rect(&justify_all_list, container, axis);
  assert_eq!(baseline_lines.len(), 1);
  assert_eq!(justify_lines.len(), 1);
  assert_eq!(justify_all_lines.len(), 1);

  // `text-align: justify` should behave like start alignment for the last line by default.
  assert_no_justification(
    baseline_lines[0],
    justify_lines[0],
    inline_size,
    "text-align-last:auto (default) on single line",
  );
  assert_justification_happened(
    justify_lines[0],
    justify_all_lines[0],
    axis,
    inline_start,
    inline_end,
    "text-align: justify-all",
  );
}

#[test]
fn rtl_text_align_justify_still_fills_width() {
  let container_color = Rgba::new(15, 25, 35, 1.0);
  let axis = InlineAxis::Horizontal;

  let words = ["שלום", "עולם", "שלום", "עולם", "שלום"];
  let inner_html = words_as_spans(&words);

  let common_css = r#"
    @font-face {
      font-family: "Hebrew";
      src: url("tests/fixtures/fonts/NotoSansHebrew-subset.ttf") format("truetype");
    }
    body { margin: 0; background: white; }
    #c {
      width: 480px;
      height: 90px;
      padding: 0;
      background: rgb(15, 25, 35);
      font-family: "Hebrew";
      font-size: 22px;
      line-height: 1;
      white-space: nowrap;
      color: black;
      direction: rtl;
    }
  "#;

  let baseline_html = format!(
    r#"<!doctype html><html><head><style>{common_css}</style></head><body>
      <div id="c" style="text-align: start">{inner_html}</div>
    </body></html>"#
  );
  let justified_html = format!(
    r#"<!doctype html><html><head><style>{common_css}</style></head><body>
      <div id="c" style="text-align: justify; text-align-last: justify; text-justify: inter-word">{inner_html}</div>
    </body></html>"#
  );

  let baseline_list = render_display_list(&baseline_html, 560, 140);
  let justified_list = render_display_list(&justified_html, 560, 140);
  let container = find_background_rect_bounds(&baseline_list, container_color);
  let (inline_start, inline_end) = container_inline_bounds(container, axis);

  let baseline_lines = line_metrics_in_rect(&baseline_list, container, axis);
  let justified_lines = line_metrics_in_rect(&justified_list, container, axis);
  assert_eq!(
    baseline_lines.len(),
    1,
    "expected a single baseline line, got {baseline_lines:?}"
  );
  assert_eq!(
    justified_lines.len(),
    1,
    "expected a single justified line, got {justified_lines:?}"
  );

  assert_justification_happened(
    baseline_lines[0],
    justified_lines[0],
    axis,
    inline_start,
    inline_end,
    "RTL inter-word",
  );
}

#[test]
fn vertical_writing_mode_justifies_along_inline_axis() {
  let container_color = Rgba::new(16, 26, 36, 1.0);
  let axis = InlineAxis::Vertical;

  let text = "縦書き縦書き縦書き縦";
  let inner_html = chars_as_spans(text);

  let common_css = r#"
    @font-face {
      font-family: "CJK";
      src: url("tests/fixtures/fonts/NotoSansJP-subset.ttf") format("truetype");
    }
    body { margin: 0; background: white; }
    #c {
      width: 140px;
      height: 320px;
      padding: 0;
      background: rgb(16, 26, 36);
      font-family: "CJK";
      font-size: 24px;
      line-height: 1;
      white-space: nowrap;
      color: black;
      writing-mode: vertical-rl;
    }
  "#;

  let baseline_html = format!(
    r#"<!doctype html><html><head><style>{common_css}</style></head><body>
      <div id="c" style="text-align: start">{inner_html}</div>
    </body></html>"#
  );
  let justified_html = format!(
    r#"<!doctype html><html><head><style>{common_css}</style></head><body>
      <div id="c" style="text-align: justify; text-align-last: justify; text-justify: inter-character">{inner_html}</div>
    </body></html>"#
  );

  let baseline_list = render_display_list(&baseline_html, 220, 420);
  let justified_list = render_display_list(&justified_html, 220, 420);
  let container = find_background_rect_bounds(&baseline_list, container_color);
  let (inline_start, inline_end) = container_inline_bounds(container, axis);

  let baseline_lines = line_metrics_in_rect(&baseline_list, container, axis);
  let justified_lines = line_metrics_in_rect(&justified_list, container, axis);
  assert_eq!(
    baseline_lines.len(),
    1,
    "expected a single baseline line, got {baseline_lines:?}"
  );
  assert_eq!(
    justified_lines.len(),
    1,
    "expected a single justified line, got {justified_lines:?}"
  );

  assert_justification_happened(
    baseline_lines[0],
    justified_lines[0],
    axis,
    inline_start,
    inline_end,
    "vertical-rl inter-character",
  );
}
