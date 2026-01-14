use crate::paint::display_list::DisplayItem;
use crate::paint::display_list_builder::DisplayListBuilder;
use crate::text::font_db::FontConfig;
use crate::text::font_loader::FontContext;
use crate::text::pipeline::ShapedRun;
use crate::tree::fragment_tree::{FragmentContent, FragmentNode};
use crate::{
  FastRender, FragmentTree, LayoutParallelism, PaintParallelism, RenderArtifactRequest,
  RenderArtifacts, RenderOptions,
};

#[derive(Clone, Copy, Debug)]
enum SpacerEdge {
  Start,
  End,
}

fn is_spacer_char(ch: char) -> bool {
  // Mirror `DisplayListBuilder::is_spacer_char`.
  ch != '\u{202F}'
    && matches!(
      unicode_general_category::get_general_category(ch),
      unicode_general_category::GeneralCategory::SpaceSeparator
    )
}

fn spacer_advance_in_runs(runs: &[ShapedRun], edge: SpacerEdge, inline_vertical: bool) -> f32 {
  // Mirror `DisplayListBuilder::spacer_advance_in_runs` so expected ranges are derived from the same
  // glyph advances that the builder uses when computing skip-spaces clip ranges.
  let mut advance: f32 = 0.0;
  match edge {
    SpacerEdge::Start => {
      for run in runs {
        for glyph in &run.glyphs {
          let idx = glyph.cluster as usize;
          let Some(ch) = run.text.get(idx..).and_then(|s| s.chars().next()) else {
            return advance.max(0.0);
          };
          if !is_spacer_char(ch) {
            return advance.max(0.0);
          }
          advance += if inline_vertical {
            if glyph.y_advance.abs() > f32::EPSILON {
              glyph.y_advance
            } else {
              glyph.x_advance
            }
          } else {
            glyph.x_advance
          };
        }
      }
    }
    SpacerEdge::End => {
      for run in runs.iter().rev() {
        for glyph in run.glyphs.iter().rev() {
          let idx = glyph.cluster as usize;
          let Some(ch) = run.text.get(idx..).and_then(|s| s.chars().next()) else {
            return advance.max(0.0);
          };
          if !is_spacer_char(ch) {
            return advance.max(0.0);
          }
          advance += if inline_vertical {
            if glyph.y_advance.abs() > f32::EPSILON {
              glyph.y_advance
            } else {
              glyph.x_advance
            }
          } else {
            glyph.x_advance
          };
        }
      }
    }
  }

  if advance.is_finite() {
    advance.max(0.0)
  } else {
    0.0
  }
}

fn spacer_advance_for_prefix(runs: &[ShapedRun], prefix_len: usize, inline_vertical: bool) -> f32 {
  // Compute spacer advance for the `[0, prefix_len)` byte range in the original text stream,
  // regardless of bidi reordering within the shaped glyph list.
  let mut advance: f32 = 0.0;

  for run in runs {
    for glyph in &run.glyphs {
      let abs_cluster = run.start.saturating_add(glyph.cluster as usize);
      if abs_cluster >= prefix_len {
        continue;
      }
      let idx = glyph.cluster as usize;
      let Some(ch) = run.text.get(idx..).and_then(|s| s.chars().next()) else {
        continue;
      };
      if !is_spacer_char(ch) {
        continue;
      }
      advance += if inline_vertical {
        if glyph.y_advance.abs() > f32::EPSILON {
          glyph.y_advance
        } else {
          glyph.x_advance
        }
      } else {
        glyph.x_advance
      };
    }
  }

  if advance.is_finite() {
    advance.max(0.0)
  } else {
    0.0
  }
}

fn render_fragment_tree(html: &str, width: u32, height: u32) -> FragmentTree {
  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("renderer");
  let options = RenderOptions::new()
    .with_viewport(width, height)
    .with_paint_parallelism(PaintParallelism::disabled())
    .with_layout_parallelism(LayoutParallelism::disabled());
  let mut artifacts = RenderArtifacts::new(RenderArtifactRequest {
    fragment_tree: true,
    ..RenderArtifactRequest::none()
  });
  renderer
    .render_html_with_options_and_artifacts(html, options, &mut artifacts)
    .expect("render html");
  artifacts.fragment_tree.take().expect("fragment tree")
}

fn collect_text_fragments<'a>(
  node: &'a FragmentNode,
  offset: crate::geometry::Point,
  out: &mut Vec<(&'a FragmentNode, crate::geometry::Rect)>,
) {
  let abs = crate::geometry::Rect::new(offset.translate(node.bounds.origin), node.bounds.size);
  if matches!(node.content, FragmentContent::Text { .. }) {
    out.push((node, abs));
  }
  for child in node.children.iter() {
    collect_text_fragments(child, abs.origin, out);
  }
}

fn shaped_runs_for_fragment<'a>(
  fragment: &'a FragmentNode,
  font_ctx: &FontContext,
) -> Vec<ShapedRun> {
  let FragmentContent::Text { text, shaped, .. } = &fragment.content else {
    return Vec::new();
  };
  if let Some(runs) = shaped.as_deref() {
    return runs.clone();
  }
  let Some(style) = fragment.style.as_deref() else {
    return Vec::new();
  };
  crate::text::pipeline::ShapingPipeline::new()
    .shape(text, style, font_ctx)
    .unwrap_or_default()
}

fn underline_segments(item: &crate::paint::display_list::TextDecorationItem) -> Vec<(f32, f32)> {
  item
    .decorations
    .iter()
    .find_map(|deco| deco.underline.as_ref().and_then(|s| s.segments.clone()))
    .unwrap_or_default()
}

fn overline_segments(item: &crate::paint::display_list::TextDecorationItem) -> Vec<(f32, f32)> {
  item
    .decorations
    .iter()
    .find_map(|deco| deco.overline.as_ref().and_then(|s| s.segments.clone()))
    .unwrap_or_default()
}

#[test]
fn text_decoration_skip_spaces_start_maps_to_physical_start_in_vertical_writing() {
  crate::testing::init_rayon_for_tests(2);

  let html = r#"<!doctype html><html><head><style>
    body { margin: 0; }
    .sample {
      font-family: "DejaVu Sans";
      font-size: 20px;
      writing-mode: vertical-rl;
      white-space: pre;
      text-decoration: underline overline;
      text-decoration-skip-ink: none;
      text-decoration-skip-spaces: start;
    }
  </style></head><body><div class="sample">  hi  </div></body></html>"#;

  let tree = render_fragment_tree(html, 240, 240);
  let font_ctx = FontContext::with_config(FontConfig::bundled_only());

  let mut text_frags = Vec::new();
  collect_text_fragments(&tree.root, crate::geometry::Point::ZERO, &mut text_frags);
  let (text_frag, text_rect) = text_frags
    .into_iter()
    .find(|(node, _)| match &node.content {
      FragmentContent::Text { text, .. } => text.contains("hi"),
      _ => false,
    })
    .expect("expected text fragment containing 'hi'");

  let runs = shaped_runs_for_fragment(text_frag, &font_ctx);
  assert!(
    !runs.is_empty(),
    "expected shaped runs for vertical text fragment"
  );
  let expected_skip_start = spacer_advance_in_runs(&runs, SpacerEdge::Start, true);
  assert!(
    expected_skip_start > 0.0,
    "expected non-zero leading spacer advance in vertical text"
  );

  let list = DisplayListBuilder::new()
    .with_font_context(font_ctx)
    .build_tree(&tree);

  let deco_item = list
    .items()
    .iter()
    .filter_map(|item| match item {
      DisplayItem::TextDecoration(deco) if deco.inline_vertical => Some(deco),
      _ => None,
    })
    .find(|deco| (deco.line_start - text_rect.y()).abs() < 0.5)
    .expect("expected vertical TextDecorationItem for sample");

  let segments = underline_segments(deco_item);
  assert_eq!(
    segments.len(),
    1,
    "expected exactly one underline segment after skip-spaces clipping, got {segments:?}"
  );
  let (start, end) = segments[0];
  assert!(
    (start - expected_skip_start).abs() < 0.05,
    "expected underline to skip physical start-side spaces in vertical writing: start={start} expected={expected_skip_start} segments={segments:?}"
  );
  assert!(
    (end - deco_item.line_width).abs() < 0.05,
    "expected underline to extend to the physical end when only start is skipped: end={end} line_width={} segments={segments:?}",
    deco_item.line_width
  );

  let segments = overline_segments(deco_item);
  assert_eq!(
    segments.len(),
    1,
    "expected exactly one overline segment after skip-spaces clipping, got {segments:?}"
  );
  let (start, end) = segments[0];
  assert!(
    (start - expected_skip_start).abs() < 0.05,
    "expected overline to skip physical start-side spaces in vertical writing: start={start} expected={expected_skip_start} segments={segments:?}"
  );
  assert!(
    (end - deco_item.line_width).abs() < 0.05,
    "expected overline to extend to the physical end when only start is skipped: end={end} line_width={} segments={segments:?}",
    deco_item.line_width
  );
}

#[test]
fn text_decoration_skip_spaces_start_clips_physical_end_in_vertical_writing_when_direction_rtl() {
  crate::testing::init_rayon_for_tests(2);

  let html = r#"<!doctype html><html><head><style>
    body { margin: 0; }
    .sample {
      font-family: "DejaVu Sans";
      font-size: 20px;
      writing-mode: vertical-rl;
      direction: rtl;
      white-space: pre;
      text-decoration: underline overline;
      text-decoration-skip-ink: none;
      text-decoration-skip-spaces: start;
    }
  </style></head><body><div class="sample">  hi  </div></body></html>"#;

  let tree = render_fragment_tree(html, 240, 240);
  let font_ctx = FontContext::with_config(FontConfig::bundled_only());

  let mut text_frags = Vec::new();
  collect_text_fragments(&tree.root, crate::geometry::Point::ZERO, &mut text_frags);
  let (text_frag, text_rect) = text_frags
    .into_iter()
    .find(|(node, _)| match &node.content {
      FragmentContent::Text { text, .. } => text.contains("hi"),
      _ => false,
    })
    .expect("expected text fragment containing 'hi'");

  let runs = shaped_runs_for_fragment(text_frag, &font_ctx);
  assert!(
    !runs.is_empty(),
    "expected shaped runs for vertical-rl direction:rtl fragment"
  );

  // With equal leading+trailing spaces, the spacer advance should be identical on both edges. This
  // keeps the expected clipped length stable without depending on which edge the shaping backend
  // considers the run start.
  let expected_skip = spacer_advance_in_runs(&runs, SpacerEdge::Start, true);
  let expected_skip_end = spacer_advance_in_runs(&runs, SpacerEdge::End, true);
  assert!(
    expected_skip > 0.0,
    "expected non-zero spacer advance at logical start in vertical-rl direction:rtl"
  );
  assert!(
    (expected_skip - expected_skip_end).abs() < 0.05,
    "expected equal spacer advances at both edges for symmetric space padding: start={expected_skip} end={expected_skip_end}"
  );

  let list = DisplayListBuilder::new()
    .with_font_context(font_ctx)
    .build_tree(&tree);

  let deco_item = list
    .items()
    .iter()
    .filter_map(|item| match item {
      DisplayItem::TextDecoration(deco) if deco.inline_vertical => Some(deco),
      _ => None,
    })
    .find(|deco| {
      (deco.line_start - text_rect.y()).abs() < 0.5
        && (deco.line_width - text_rect.height()).abs() < 0.5
    })
    .expect("expected vertical TextDecorationItem for sample");

  let expected_end = deco_item.line_width - expected_skip;

  let segments = underline_segments(deco_item);
  assert_eq!(
    segments.len(),
    1,
    "expected exactly one underline segment after skip-spaces clipping, got {segments:?}"
  );
  let (start, end) = segments[0];
  assert!(
    start.abs() < 0.05,
    "expected skip-spaces:start not to clip the physical start when direction:rtl maps start->physical end: start={start} segments={segments:?}"
  );
  assert!(
    (end - expected_end).abs() < 0.05,
    "expected skip-spaces:start to clip physical end when direction:rtl: end={end} expected_end={expected_end} line_width={} skip={expected_skip} segments={segments:?}",
    deco_item.line_width
  );

  let segments = overline_segments(deco_item);
  assert_eq!(
    segments.len(),
    1,
    "expected exactly one overline segment after skip-spaces clipping, got {segments:?}"
  );
  let (start, end) = segments[0];
  assert!(
    start.abs() < 0.05,
    "expected skip-spaces:start not to clip the physical start of overline when direction:rtl maps start->physical end: start={start} segments={segments:?}"
  );
  assert!(
    (end - expected_end).abs() < 0.05,
    "expected skip-spaces:start to clip physical end of overline when direction:rtl: end={end} expected_end={expected_end} line_width={} skip={expected_skip} segments={segments:?}",
    deco_item.line_width
  );
}

#[test]
fn text_decoration_skip_spaces_end_clips_physical_end_after_bidi_reordering() {
  crate::testing::init_rayon_for_tests(2);

  let html = r#"<!doctype html><html><head><style>
    body { margin: 0; }
    .sample {
      font-family: "DejaVu Sans";
      font-size: 20px;
      direction: ltr;
      white-space: pre;
      text-decoration: underline overline;
      text-decoration-skip-ink: none;
      text-decoration-skip-spaces: end;
    }
    .rtl {
      direction: rtl;
      unicode-bidi: isolate;
    }
  </style></head><body><div class="sample">A<span class="rtl">  אב</span></div></body></html>"#;

  let tree = render_fragment_tree(html, 320, 120);
  let font_ctx = FontContext::with_config(FontConfig::bundled_only());

  let mut text_frags = Vec::new();
  collect_text_fragments(&tree.root, crate::geometry::Point::ZERO, &mut text_frags);
  let (rtl_frag, rtl_rect) = text_frags
    .into_iter()
    .find(|(node, _)| match &node.content {
      FragmentContent::Text { text, .. } => text.contains('א'),
      _ => false,
    })
    .expect("expected RTL text fragment containing Hebrew");
  let rtl_style = rtl_frag
    .style
    .as_deref()
    .expect("expected RTL fragment to carry style");
  assert_eq!(
    rtl_style.direction,
    crate::style::types::Direction::Rtl,
    "expected RTL fragment to have direction: rtl"
  );

  let runs = shaped_runs_for_fragment(rtl_frag, &font_ctx);
  assert!(!runs.is_empty(), "expected shaped runs for RTL fragment");

  let FragmentContent::Text { text, .. } = &rtl_frag.content else {
    panic!("expected RTL fragment to be a text fragment");
  };
  let text = text.as_ref();
  let leading_end = text
    .char_indices()
    .find(|(_, ch)| !is_spacer_char(*ch))
    .map(|(idx, _)| idx)
    .unwrap_or(text.len());
  assert!(
    leading_end > 0,
    "expected RTL fragment to contain leading spaces, got text={text:?}"
  );

  // In an RTL span in an LTR line, those *leading* spaces can end up on the physical inline-end
  // edge after bidi reordering. `text-decoration-skip-spaces: end` should therefore clip by the
  // logical-leading spacer advance.
  let expected_skip_end = spacer_advance_for_prefix(&runs, leading_end, false);
  assert!(
    expected_skip_end > 0.0,
    "expected non-zero spacer advance for logical-leading spaces in RTL fragment"
  );

  let list = DisplayListBuilder::new()
    .with_font_context(font_ctx)
    .build_tree(&tree);

  let deco_item = list
    .items()
    .iter()
    .filter_map(|item| match item {
      DisplayItem::TextDecoration(deco) if !deco.inline_vertical => Some(deco),
      _ => None,
    })
    .find(|deco| {
      (deco.line_start - rtl_rect.x()).abs() < 0.5
        && (deco.line_width - rtl_rect.width()).abs() < 0.5
    })
    .expect("expected horizontal TextDecorationItem for RTL fragment");

  let segments = underline_segments(deco_item);
  assert_eq!(
    segments.len(),
    1,
    "expected exactly one underline segment after skip-spaces clipping, got {segments:?}"
  );
  let (start, end) = segments[0];
  assert!(
    start.abs() < 0.05,
    "expected skip-spaces:end not to clip the physical start: start={start} segments={segments:?}"
  );
  let expected_end = deco_item.line_width - expected_skip_end;
  assert!(
    (end - expected_end).abs() < 0.05,
    "expected skip-spaces:end to clip physical end-side spaces after bidi reordering: end={end} expected_end={expected_end} line_width={} skip={expected_skip_end} segments={segments:?}",
    deco_item.line_width
  );

  let segments = overline_segments(deco_item);
  assert_eq!(
    segments.len(),
    1,
    "expected exactly one overline segment after skip-spaces clipping, got {segments:?}"
  );
  let (start, end) = segments[0];
  assert!(
    start.abs() < 0.05,
    "expected skip-spaces:end not to clip the physical start of overline: start={start} segments={segments:?}"
  );
  assert!(
    (end - expected_end).abs() < 0.05,
    "expected skip-spaces:end to clip physical end-side spaces for overline after bidi reordering: end={end} expected_end={expected_end} line_width={} skip={expected_skip_end} segments={segments:?}",
    deco_item.line_width
  );
}
