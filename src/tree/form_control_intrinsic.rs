use crate::geometry::Size;
use crate::layout::contexts::inline::baseline::compute_line_height_with_metrics_viewport;
use crate::layout::contexts::inline::line_builder::TextItem;
use crate::layout::utils::{
  resolve_font_relative_length, resolve_length_with_percentage_metrics, resolve_scrollbar_width,
};
use crate::style::types::Appearance;
use crate::style::types::FieldSizing;
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

// These values are mirrored from the form-control painters so content-based intrinsic sizing leaves
// space for the same affordances.
const TEXT_CONTROL_NUMBER_AFFORDANCE_SPACE_PX: f32 = 14.0;
const TEXT_CONTROL_DATE_AFFORDANCE_SPACE_PX: f32 = 12.0;

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
    if style.letter_spacing == 0.0 && style.word_spacing == 0.0 {
      if let Ok(runs) = shaper.shape_arc(text, style, font_context) {
        if !runs.is_empty() {
          let width: f32 = runs.iter().map(|run| run.advance).sum();
          if width.is_finite() && width >= 0.0 {
            return width;
          }
        }
      }
    } else if let Ok(mut runs) = shaper.shape(text, style, font_context) {
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
  let line_height = compute_line_height_with_metrics_viewport(
    style,
    metrics_scaled,
    Some(viewport),
    font_context.root_font_metrics(),
  );

  match &control.control {
    FormControlKind::Text {
      value,
      placeholder,
      placeholder_style,
      size_attr,
      kind,
      ..
    } => {
      if matches!(style.field_sizing, FieldSizing::Content)
        && matches!(
          kind,
          TextControlKind::Plain
            | TextControlKind::Password
            | TextControlKind::Number
            | TextControlKind::Date
        )
      {
        let value_is_empty = value.is_empty();
        let mut measure_style = style;

        let measure_text: Option<std::borrow::Cow<'_, str>> = if !value_is_empty {
          match kind {
            TextControlKind::Password => {
              let mask_len = value.chars().count();
              Some(std::borrow::Cow::Owned("•".repeat(mask_len)))
            }
            _ => Some(std::borrow::Cow::Borrowed(value.as_str())),
          }
        } else if let Some(ph) = placeholder.as_deref().filter(|p| !p.is_empty()) {
          if let Some(ph_style) = placeholder_style.as_deref() {
            measure_style = ph_style;
          }
          Some(std::borrow::Cow::Borrowed(ph))
        } else if matches!(kind, TextControlKind::Date) {
          // Mirror the form-control painter's implicit placeholder for date-like inputs.
          if let Some(ph_style) = placeholder_style.as_deref() {
            measure_style = ph_style;
          }
          Some(std::borrow::Cow::Borrowed("yyyy-mm-dd"))
        } else {
          None
        };

        if let Some(text) = measure_text.as_ref() {
          let mut width = measure_text_width(
            text.as_ref(),
            measure_style,
            font_context,
            shaper,
            char_width,
          );
          if !matches!(control.appearance, Appearance::None) {
            match kind {
              TextControlKind::Number => width += TEXT_CONTROL_NUMBER_AFFORDANCE_SPACE_PX,
              TextControlKind::Date => width += TEXT_CONTROL_DATE_AFFORDANCE_SPACE_PX,
              _ => {}
            }
          }
          return Size::new(width, line_height);
        }
      }

      let default_cols = match kind {
        TextControlKind::Date | TextControlKind::Number => 20,
        _ => 20,
      } as f32;
      let cols = size_attr.unwrap_or(default_cols as u32) as f32;
      Size::new(char_width * cols.max(1.0), line_height)
    }
    FormControlKind::TextArea {
      value,
      placeholder,
      placeholder_style,
      rows,
      cols,
      ..
    } => {
      if matches!(style.field_sizing, FieldSizing::Content) {
        let mut max_width = 0.0f32;
        let mut line_count = 0usize;
        let mut measure_style = style;

        let raw_text: &str = if !value.is_empty() {
          value
        } else if let Some(ph) = placeholder.as_deref().filter(|p| !p.is_empty()) {
          if let Some(ph_style) = placeholder_style.as_deref() {
            measure_style = ph_style;
          }
          ph
        } else {
          value
        };

        for line in raw_text.split('\n') {
          line_count += 1;
          max_width = max_width.max(measure_text_width(
            line,
            measure_style,
            font_context,
            shaper,
            char_width,
          ));
        }
        if line_count == 0 {
          line_count = 1;
        }
        if max_width == 0.0 {
          // Avoid collapsing an empty textarea to 0px: keep the legacy `cols`-based inline size
          // when there is no value/placeholder content to measure.
          let col_count = cols.unwrap_or(20) as f32;
          max_width = char_width * col_count.max(1.0);
        }

        return Size::new(max_width, line_height * line_count as f32);
      }

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
    FormControlKind::Progress { .. } | FormControlKind::Meter { .. } => {
      // Browsers generally size these controls around 10em × 1em when the author doesn't specify
      // explicit dimensions. Use font-size (em) directly rather than line-height so the intrinsic
      // height tracks the element's font size even under large leading.
      let em = style.font_size.max(0.0);
      Size::new(em * 10.0, em)
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

#[cfg(test)]
mod tests {
  use super::*;
  use crate::css::parser::parse_stylesheet;
  use crate::dom;
  use crate::style::cascade;
  use crate::text::font_loader::FontContext;
  use crate::tree::box_generation::generate_box_tree;
  use crate::tree::box_tree::{BoxType, ReplacedType};

  fn find_textarea_form_control<'a>(
    node: &'a crate::tree::box_tree::BoxNode,
  ) -> Option<(&'a FormControl, &'a ComputedStyle)> {
    if let BoxType::Replaced(repl) = &node.box_type {
      if let ReplacedType::FormControl(control) = &repl.replaced_type {
        if matches!(control.control, FormControlKind::TextArea { .. }) {
          return Some((control, &node.style));
        }
      }
    }
    for child in node.children.iter() {
      if let Some(hit) = find_textarea_form_control(child) {
        return Some(hit);
      }
    }
    None
  }

  #[test]
  fn textarea_intrinsic_height_respects_rows_and_line_height() {
    // google.com search uses a <textarea rows="1"> with explicit line-height. If we ignore either,
    // the control becomes too tall and breaks the search box layout.
    let html = r#"
      <html>
        <body>
          <textarea class="gLFyf" rows="1"></textarea>
        </body>
      </html>
    "#;

    let dom = dom::parse_html(html).expect("parse html");
    // Include the competing `.gLFyf { line-height: 40px }` rule from the real google.com fixture so
    // this test catches any bug where the `textarea.gLFyf` override is not applied.
    let stylesheet = parse_stylesheet(
      ".gLFyf,.Rd7rGe,.YacQv,.jOAti{font:16px Google Sans,Roboto,Arial,sans-serif;line-height:40px;font-size:16px;flex:100%;}textarea.gLFyf,.Rd7rGe,.YacQv,.jOAti{line-height:22px;border-bottom:8px solid transparent;padding-top:14px;overflow-x:hidden}",
    )
    .expect("parse stylesheet");
    let styled = cascade::apply_styles(&dom, &stylesheet);
    let box_tree = generate_box_tree(&styled).expect("box generation");
    let (control, style) =
      find_textarea_form_control(&box_tree.root).expect("textarea form control");

    let viewport = Size::new(800.0, 600.0);
    let font_context = FontContext::new();
    let size =
      intrinsic_content_size_for_form_control(control, style, viewport, None, &font_context, None);

    assert!(
      (size.height - 22.0).abs() < 0.01,
      "expected 22px content height (rows=1, line-height=22px), got {size:?}",
    );
  }
}
