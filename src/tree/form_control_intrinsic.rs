use crate::geometry::Size;
use crate::layout::contexts::inline::baseline::compute_line_height_with_metrics_viewport;
use crate::layout::contexts::inline::line_builder::TextItem;
use crate::layout::utils::{
  resolve_font_relative_length, resolve_length_with_percentage_metrics, resolve_scrollbar_width,
};
use crate::style::types::Appearance;
use crate::style::values::{Length, LengthUnit};
use crate::style::ComputedStyle;
use crate::text::font_db::ScaledMetrics;
use crate::text::font_loader::FontContext;
use crate::text::pipeline::ShapingPipeline;
use crate::tree::box_tree::{FormControl, FormControlKind, SelectItem, TextControlKind};

const DEFAULT_RANGE_TRACK_LENGTH_CH: f32 = 12.0;
const DEFAULT_RANGE_THUMB_SIZE_PX: f32 = 16.0;
const DEFAULT_RANGE_TRACK_HEIGHT_PX: f32 = 4.0;

const DEFAULT_FILE_WIDTH_CH: f32 = 24.0;
const DEFAULT_FILE_MIN_HEIGHT_PX: f32 = 16.0;
const FILE_INNER_INSET_PX: f32 = 2.0;
const FILE_BUTTON_GAP_PX: f32 = 6.0;

const DEFAULT_SELECT_MIN_LABEL_CH: f32 = 4.0;
const SELECT_DROPDOWN_ARROW_SPACE_PX: f32 = 14.0;
const SELECT_DROPDOWN_INNER_INSET_PX: f32 = 2.0;
const SELECT_LISTBOX_MAX_INDENT_PX: f32 = 10.0;

fn measure_text_width(
  text: &str,
  style: &ComputedStyle,
  font_context: &FontContext,
  shaper: Option<&ShapingPipeline>,
  char_width: f32,
) -> f32 {
  if text.is_empty() {
    return 0.0;
  }

  if let Some(shaper) = shaper {
    if let Ok(mut runs) = shaper.shape(text, style, font_context) {
      if !runs.is_empty() {
        TextItem::apply_spacing_to_runs(&mut runs, text, style.letter_spacing, style.word_spacing);
        let width: f32 = runs.iter().map(|run| run.advance).sum();
        if width.is_finite() && width >= 0.0 {
          return width;
        }
      }
    }
  }

  char_width * text.chars().count() as f32
}

fn resolve_positive_length_px(
  style: &ComputedStyle,
  length: Option<Length>,
  viewport: Size,
  font_context: &FontContext,
) -> Option<f32> {
  let length = length?;
  resolve_length_with_percentage_metrics(
    length,
    None,
    viewport,
    style.font_size,
    style.root_font_size,
    Some(style),
    Some(font_context),
  )
  .filter(|px| px.is_finite() && *px > 0.0)
}

pub(crate) fn intrinsic_content_size_for_form_control(
  control: &FormControl,
  style: &ComputedStyle,
  viewport: Size,
  metrics_scaled: Option<&ScaledMetrics>,
  font_context: &FontContext,
  shaper: Option<&ShapingPipeline>,
) -> Size {
  let char_width =
    resolve_font_relative_length(Length::new(1.0, LengthUnit::Ch), style, font_context);
  let line_height =
    compute_line_height_with_metrics_viewport(style, metrics_scaled, Some(viewport));

  match &control.control {
    FormControlKind::Text {
      size_attr, kind, ..
    } => {
      let default_cols = match kind {
        TextControlKind::Date | TextControlKind::Number => 20,
        _ => 20,
      } as f32;
      let cols = size_attr.unwrap_or(default_cols as u32) as f32;
      Size::new(char_width * cols.max(1.0), line_height)
    }
    FormControlKind::TextArea { rows, cols, .. } => {
      let row_count = rows.unwrap_or(2) as f32;
      let col_count = cols.unwrap_or(20) as f32;
      Size::new(
        char_width * col_count.max(1.0),
        line_height * row_count.max(1.0),
      )
    }
    FormControlKind::Button { label } => {
      let width = measure_text_width(label, style, font_context, shaper, char_width);
      Size::new(width.max(char_width), line_height)
    }
    FormControlKind::Select(select) => {
      let is_listbox = select.multiple || select.size > 1;
      let mut max_label_width = 0.0f32;
      let mut saw_label = false;

      for item in select.items.iter() {
        match item {
          SelectItem::Option { label, .. } => {
            saw_label = true;
            max_label_width = max_label_width.max(measure_text_width(
              label,
              style,
              font_context,
              shaper,
              char_width,
            ));
          }
          SelectItem::OptGroupLabel { label, .. } if is_listbox => {
            saw_label = true;
            max_label_width = max_label_width.max(measure_text_width(
              label,
              style,
              font_context,
              shaper,
              char_width,
            ));
          }
          _ => {}
        }
      }

      let min_label_width = char_width * DEFAULT_SELECT_MIN_LABEL_CH;
      if !saw_label {
        max_label_width = min_label_width;
      } else {
        max_label_width = max_label_width.max(min_label_width);
      }

      let scrollbar = if is_listbox && select.items.len() as u32 > select.size.max(1) {
        resolve_scrollbar_width(style)
      } else {
        0.0
      };

      let arrow = if !is_listbox && !matches!(control.appearance, Appearance::None) {
        SELECT_DROPDOWN_ARROW_SPACE_PX
      } else {
        0.0
      };

      let extra = if is_listbox {
        SELECT_LISTBOX_MAX_INDENT_PX
      } else {
        SELECT_DROPDOWN_INNER_INSET_PX * 2.0
      };

      let height = if is_listbox {
        line_height * select.size.max(1) as f32
      } else {
        line_height
      };

      Size::new(max_label_width + scrollbar + arrow + extra, height)
    }
    FormControlKind::Checkbox { .. } => {
      let edge = (style.font_size * 1.1).clamp(12.0, 20.0);
      Size::new(edge, edge)
    }
    FormControlKind::Range { .. } => {
      let thumb_style = control.slider_thumb_style.as_deref();
      let track_style = control.slider_track_style.as_deref();

      let thumb_width = thumb_style
        .and_then(|s| resolve_positive_length_px(s, s.width, viewport, font_context))
        .unwrap_or(DEFAULT_RANGE_THUMB_SIZE_PX);
      let thumb_height = thumb_style
        .and_then(|s| resolve_positive_length_px(s, s.height, viewport, font_context))
        .unwrap_or(DEFAULT_RANGE_THUMB_SIZE_PX);
      let track_height = track_style
        .and_then(|s| resolve_positive_length_px(s, s.height, viewport, font_context))
        .unwrap_or(DEFAULT_RANGE_TRACK_HEIGHT_PX);

      let width = (char_width * DEFAULT_RANGE_TRACK_LENGTH_CH).max(thumb_width);
      let height = thumb_height.max(track_height);
      Size::new(width, height)
    }
    FormControlKind::Color { .. } => Size::new(
      (line_height * 2.0).max(char_width * 6.0),
      line_height.max(16.0_f32.min(line_height * 1.2)),
    ),
    FormControlKind::File { .. } => {
      let button_style = control.file_selector_button_style.as_deref();

      let min_height = line_height.max(DEFAULT_FILE_MIN_HEIGHT_PX);
      let button_height = button_style
        .and_then(|s| resolve_positive_length_px(s, s.height, viewport, font_context))
        .map(|h| h + FILE_INNER_INSET_PX * 2.0)
        .unwrap_or(0.0);
      let height = min_height.max(button_height);

      let default_width = char_width * DEFAULT_FILE_WIDTH_CH;
      let button_width = button_style
        .and_then(|s| resolve_positive_length_px(s, s.width, viewport, font_context))
        .map(|w| w + FILE_BUTTON_GAP_PX + FILE_INNER_INSET_PX * 2.0)
        .unwrap_or(0.0);
      let width = default_width.max(button_width);

      Size::new(width, height)
    }
    FormControlKind::Unknown { .. } => Size::new(char_width * 12.0, line_height),
  }
}

