use fastrender::geometry::Rect;
use fastrender::paint::display_list::DisplayItem;
use fastrender::paint::display_list_builder::DisplayListBuilder;
use fastrender::style::types::{Appearance, CaretColor, TextAlign};
use fastrender::tree::box_tree::{FormControl, FormControlKind, ReplacedType, TextControlKind};
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use fastrender::{ComputedStyle, Rgba};
use std::sync::Arc;

fn text_input_fragment(style: ComputedStyle, focused: bool, value: &str) -> FragmentNode {
  let control = FormControl {
    control: FormControlKind::Text {
      value: value.to_string(),
      placeholder: None,
      placeholder_style: None,
      size_attr: None,
      kind: TextControlKind::Plain,
    },
    appearance: Appearance::Auto,
    disabled: false,
    focused,
    focus_visible: false,
    required: false,
    invalid: false,
    placeholder_style: None,
    slider_thumb_style: None,
    slider_track_style: None,
    file_selector_button_style: None,
  };

  FragmentNode::new_with_style(
    Rect::from_xywh(0.0, 0.0, 200.0, 30.0),
    FragmentContent::Replaced {
      replaced_type: ReplacedType::FormControl(control),
      box_id: None,
    },
    vec![],
    Arc::new(style),
  )
}

fn min_text_origin_x(items: &[DisplayItem]) -> Option<f32> {
  items
    .iter()
    .filter_map(|item| match item {
      DisplayItem::Text(text) if text.glyphs.len() > 1 => Some(text.origin.x),
      _ => None,
    })
    .fold(None, |acc, x| Some(acc.map_or(x, |v| v.min(x))))
}

fn caret_x(items: &[DisplayItem], caret_color: Rgba) -> Option<f32> {
  items.iter().find_map(|item| match item {
    DisplayItem::FillRect(fill)
      if fill.color == caret_color && (fill.rect.width() - 1.0).abs() < 0.01 =>
    {
      Some(fill.rect.x())
    }
    _ => None,
  })
}

#[test]
fn text_input_respects_text_align_right() {
  let mut left_style = ComputedStyle::default();
  left_style.font_size = 16.0;
  left_style.color = Rgba::GREEN;
  left_style.text_align = TextAlign::Left;

  let mut right_style = left_style.clone();
  right_style.text_align = TextAlign::Right;

  let left_fragment = text_input_fragment(left_style, false, "hello");
  let right_fragment = text_input_fragment(right_style, false, "hello");

  let left_list = DisplayListBuilder::new().build(&left_fragment);
  let right_list = DisplayListBuilder::new().build(&right_fragment);

  let left_min_x = min_text_origin_x(left_list.items()).expect("expected a text item for value");
  let right_min_x = min_text_origin_x(right_list.items()).expect("expected a text item for value");

  assert!(
    right_min_x > left_min_x + 20.0,
    "expected right-aligned input text to start further right (left_min_x={left_min_x}, right_min_x={right_min_x})"
  );
}

#[test]
fn focused_empty_text_input_emits_visible_right_aligned_caret() {
  let caret_color = Rgba::new(12, 34, 56, 1.0);

  let mut left_style = ComputedStyle::default();
  left_style.font_size = 16.0;
  left_style.color = Rgba::BLACK;
  left_style.caret_color = CaretColor::Color(caret_color);
  left_style.text_align = TextAlign::Left;

  let mut right_style = left_style.clone();
  right_style.text_align = TextAlign::Right;

  let left_fragment = text_input_fragment(left_style, true, "");
  let right_fragment = text_input_fragment(right_style, true, "");

  let left_list = DisplayListBuilder::new().build(&left_fragment);
  let right_list = DisplayListBuilder::new().build(&right_fragment);

  let left_caret_x = caret_x(left_list.items(), caret_color).expect("expected a caret fill rect");
  let right_caret_x = caret_x(right_list.items(), caret_color).expect("expected a caret fill rect");

  assert!(
    right_caret_x > left_caret_x + 20.0,
    "expected right-aligned caret to be further right (left={left_caret_x}, right={right_caret_x})"
  );
  assert!(
    right_caret_x < 200.0 - 0.01,
    "expected caret to be clamped inside the control width (right_caret_x={right_caret_x})"
  );
}
