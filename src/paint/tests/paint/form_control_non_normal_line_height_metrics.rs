use crate::geometry::Rect;
use crate::layout::contexts::inline::line_builder::TextItem as InlineTextItem;
use crate::paint::display_list::DisplayItem;
use crate::paint::display_list_builder::DisplayListBuilder;
use crate::style::types::{Appearance, CaretColor, LineHeight};
use crate::style::values::Length;
use crate::text::caret::CaretAffinity;
use crate::text::font_db::FontConfig;
use crate::text::font_loader::FontContext;
use crate::tree::box_tree::{FormControl, FormControlKind, ReplacedType, TextControlKind};
use crate::tree::fragment_tree::{FragmentContent, FragmentNode};
use crate::{ComputedStyle, Rgba, ShapingPipeline};
use std::collections::HashSet;
use std::sync::Arc;

fn selection_rect_height(items: &[DisplayItem], selection_color: Rgba) -> Option<f32> {
  items
    .iter()
    .filter_map(|item| match item {
      DisplayItem::FillRect(fill) if fill.color == selection_color => Some(fill.rect.height()),
      _ => None,
    })
    .fold(None, |acc, h| Some(acc.map_or(h, |v| v.max(h))))
}

fn focused_form_control_fragment(style: ComputedStyle, kind: FormControlKind) -> FragmentNode {
  let control = FormControl {
    control: kind,
    appearance: Appearance::Auto,
    disabled: false,
    focused: true,
    focus_visible: false,
    required: false,
    invalid: false,
    placeholder_style: None,
    slider_thumb_style: None,
    slider_track_style: None,
    progress_bar_style: None,
    progress_value_style: None,
    meter_bar_style: None,
    meter_optimum_value_style: None,
    meter_suboptimum_value_style: None,
    meter_even_less_good_value_style: None,
    file_selector_button_style: None,
    ime_preedit: None,
  };

  FragmentNode::new_with_style(
    // Use a tall control so caret/selection rects are not clipped after applying `line-height`.
    Rect::from_xywh(0.0, 0.0, 400.0, 120.0),
    FragmentContent::Replaced {
      replaced_type: ReplacedType::FormControl(control),
      box_id: None,
    },
    vec![],
    Arc::new(style),
  )
}

#[test]
fn form_controls_non_normal_line_height_do_not_inflate_selection_metrics_from_fallback_runs() {
  let font_ctx = FontContext::with_config(FontConfig::bundled_only());
  let pipeline = ShapingPipeline::new();

  let selection_color = Rgba::new(0, 120, 215, 0.35);

  let mut style = ComputedStyle::default();
  style.font_family = vec!["sans-serif".to_string()].into();
  style.font_size = 32.0;
  style.line_height = LineHeight::Length(Length::px(40.0));
  style.color = Rgba::BLACK;
  style.caret_color = CaretColor::Color(Rgba::new(5, 6, 7, 1.0));

  let line_height = 40.0;

  // Determine the primary font metrics by shaping an ASCII sample (forces the primary face).
  let primary_runs = pipeline
    .shape("A", &style, &font_ctx)
    .expect("shape primary");
  assert!(
    !primary_runs.is_empty(),
    "expected shaping an ASCII sample to produce runs"
  );
  let primary_run = &primary_runs[0];
  let primary_scaled = font_ctx
    .get_scaled_metrics_with_variations(
      primary_run.font.as_ref(),
      primary_run.font_size,
      &primary_run.variations,
    )
    .expect("scaled metrics for primary font");

  let expected_metrics = InlineTextItem::metrics_from_first_available_font(
    Some(&primary_scaled),
    line_height,
    style.font_size,
  );
  let expected_height = expected_metrics.ascent + expected_metrics.descent;

  // Pick a multi-script sample that forces font fallback *and* produces different max-run
  // ascent/descent than the primary font. This makes the test robust to tweaks in the bundled
  // font set.
  let candidates = [
    "A界",
    "A界مرحبا",
    "A界שלום",
    "A界नमस्ते",
    "A界ไทย",
    "A界∫",
    "A界←→★",
  ];

  let mut chosen_text: Option<&str> = None;
  let mut inflated_height: f32 = 0.0;

  for text in candidates {
    let runs = match pipeline.shape(text, &style, &font_ctx) {
      Ok(runs) => runs,
      Err(_) => continue,
    };
    if runs.is_empty() {
      continue;
    }
    let families: HashSet<&str> = runs.iter().map(|run| run.font.family.as_str()).collect();
    if families.len() < 2 {
      continue;
    }

    let inflated_metrics =
      InlineTextItem::metrics_from_runs(&font_ctx, &runs, line_height, style.font_size);
    let height = inflated_metrics.ascent + inflated_metrics.descent;
    if height - expected_height > 1.0 {
      chosen_text = Some(text);
      inflated_height = height;
      break;
    }
  }

  let text = chosen_text.expect(
    "expected at least one multi-script sample to trigger fallback font metric inflation; \
     update the candidate list if the bundled font set changes",
  );
  let char_len = text.chars().count();
  assert!(char_len > 0);

  // Regression guard: the fallback-run scan must differ from the primary metrics. Without that,
  // this test would be unable to detect the bug.
  assert!(
    inflated_height - expected_height > 1.0,
    "expected inflated caret/selection height to exceed primary metrics (expected_height={expected_height:.2}, inflated_height={inflated_height:.2}, text={text:?})"
  );

  // Single-line text input selection height should be derived from primary font metrics (not a max
  // over fallback runs) when `line-height` is authored (non-normal).
  let input_kind = FormControlKind::Text {
    value: text.to_string(),
    placeholder: None,
    placeholder_style: None,
    size_attr: None,
    kind: TextControlKind::Plain,
    caret: char_len,
    caret_affinity: CaretAffinity::Downstream,
    selection: Some((0, char_len)),
  };
  let input_fragment = focused_form_control_fragment(style.clone(), input_kind);
  let input_list = DisplayListBuilder::new()
    .with_font_context(font_ctx.clone())
    .build(&input_fragment);
  let input_sel_height = selection_rect_height(input_list.items(), selection_color)
    .expect("expected selection rect for input");
  assert!(
    (input_sel_height - expected_height).abs() < 0.2,
    "expected input selection height to match primary font metrics (expected={expected_height:.2}, got={input_sel_height:.2}, inflated={inflated_height:.2}, text={text:?})"
  );

  // Multi-line textarea uses the same selection metric logic; ensure it matches the same expected
  // primary font height.
  let textarea_kind = FormControlKind::TextArea {
    value: text.to_string(),
    placeholder: None,
    placeholder_style: None,
    rows: None,
    cols: None,
    caret: char_len,
    caret_affinity: CaretAffinity::Downstream,
    selection: Some((0, char_len)),
  };
  let textarea_fragment = focused_form_control_fragment(style, textarea_kind);
  let textarea_list = DisplayListBuilder::new()
    .with_font_context(font_ctx)
    .build(&textarea_fragment);
  let textarea_sel_height = selection_rect_height(textarea_list.items(), selection_color)
    .expect("expected selection rect for textarea");
  assert!(
    (textarea_sel_height - expected_height).abs() < 0.2,
    "expected textarea selection height to match primary font metrics (expected={expected_height:.2}, got={textarea_sel_height:.2}, inflated={inflated_height:.2}, text={text:?})"
  );
}
