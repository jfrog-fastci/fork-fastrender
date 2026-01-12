use crate::geometry::Rect;
use crate::paint::display_list::DisplayItem;
use crate::paint::display_list_builder::DisplayListBuilder;
use crate::style::types::{Appearance, Direction};
use crate::tree::box_tree::{
  FormControl, FormControlKind, ReplacedType, SelectControl, SelectItem,
};
use crate::tree::fragment_tree::{FragmentContent, FragmentNode};
use crate::{ComputedStyle, Rgba};
use std::sync::Arc;

fn fragment_for_control(style: ComputedStyle, control: FormControl, rect: Rect) -> FragmentNode {
  FragmentNode::new_with_style(
    rect,
    FragmentContent::Replaced {
      replaced_type: ReplacedType::FormControl(control),
      box_id: None,
    },
    vec![],
    Arc::new(style),
  )
}

fn text_origin_x_by_glyph_count<F>(items: &[DisplayItem], predicate: F) -> Option<f32>
where
  F: Fn(usize) -> bool,
{
  items.iter().find_map(|item| match item {
    DisplayItem::Text(text) if predicate(text.glyphs.len()) => Some(text.origin.x),
    _ => None,
  })
}

#[test]
fn select_dropdown_arrow_mirrors_in_rtl() {
  let select = SelectControl {
    multiple: false,
    size: 1,
    items: Arc::new(vec![SelectItem::Option {
      node_id: 0,
      label: "Option".to_string(),
      value: "value".to_string(),
      selected: true,
      disabled: false,
      in_optgroup: false,
    }]),
    selected: vec![0],
  };

  let control = || FormControl {
    control: FormControlKind::Select(select.clone()),
    appearance: Appearance::Auto,
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
    disabled: false,
    focused: false,
    focus_visible: false,
    required: false,
    invalid: false,
    ime_preedit: None,
  };

  let mut ltr_style = ComputedStyle::default();
  ltr_style.font_size = 16.0;
  ltr_style.direction = Direction::Ltr;
  let mut rtl_style = ltr_style.clone();
  rtl_style.direction = Direction::Rtl;

  let rect = Rect::from_xywh(0.0, 0.0, 200.0, 30.0);
  let ltr_fragment = fragment_for_control(ltr_style, control(), rect);
  let rtl_fragment = fragment_for_control(rtl_style, control(), rect);

  let ltr_list = DisplayListBuilder::new().build(&ltr_fragment);
  let rtl_list = DisplayListBuilder::new().build(&rtl_fragment);

  let ltr_arrow_x = text_origin_x_by_glyph_count(ltr_list.items(), |n| n == 1)
    .expect("expected select dropdown arrow text item");
  let rtl_arrow_x = text_origin_x_by_glyph_count(rtl_list.items(), |n| n == 1)
    .expect("expected select dropdown arrow text item");

  assert!(
    ltr_arrow_x > rtl_arrow_x + 100.0,
    "expected select arrow origin to mirror (ltr={ltr_arrow_x}, rtl={rtl_arrow_x})"
  );
  assert!(
    ltr_arrow_x > 150.0,
    "expected LTR select arrow to be on the right side (x={ltr_arrow_x})"
  );
  assert!(
    rtl_arrow_x < 50.0,
    "expected RTL select arrow to be on the left side (x={rtl_arrow_x})"
  );
}

#[test]
fn range_thumb_mirrors_in_rtl_at_min() {
  let control = |value| FormControl {
    control: FormControlKind::Range {
      value,
      min: 0.0,
      max: 100.0,
    },
    appearance: Appearance::Auto,
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
    disabled: false,
    focused: false,
    focus_visible: false,
    required: false,
    invalid: false,
    ime_preedit: None,
  };

  let mut ltr_style = ComputedStyle::default();
  ltr_style.direction = Direction::Ltr;
  let mut rtl_style = ltr_style.clone();
  rtl_style.direction = Direction::Rtl;

  let rect = Rect::from_xywh(0.0, 0.0, 200.0, 30.0);
  let ltr_fragment = fragment_for_control(ltr_style, control(0.0), rect);
  let rtl_fragment = fragment_for_control(rtl_style, control(0.0), rect);

  let ltr_list = DisplayListBuilder::new().build(&ltr_fragment);
  let rtl_list = DisplayListBuilder::new().build(&rtl_fragment);

  let thumb_color = Rgba::rgb(255, 255, 255);
  let thumb_rect = |items: &[DisplayItem]| {
    items.iter().find_map(|item| match item {
      DisplayItem::FillRoundedRect(fill) if fill.color == thumb_color => Some(fill.rect),
      _ => None,
    })
  };

  let ltr_thumb = thumb_rect(ltr_list.items()).expect("expected range thumb rect");
  let rtl_thumb = thumb_rect(rtl_list.items()).expect("expected range thumb rect");

  assert!(
    ltr_thumb.x() < 20.0,
    "expected LTR range thumb to be near the left edge (x={})",
    ltr_thumb.x()
  );
  assert!(
    rtl_thumb.max_x() > 180.0,
    "expected RTL range thumb to be near the right edge (max_x={})",
    rtl_thumb.max_x()
  );
}

#[test]
fn file_input_button_and_label_mirror_in_rtl() {
  let control = FormControl {
    control: FormControlKind::File {
      value: Some("x".to_string()),
    },
    appearance: Appearance::Auto,
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
    disabled: false,
    focused: false,
    focus_visible: false,
    required: false,
    invalid: false,
    ime_preedit: None,
  };

  let mut ltr_style = ComputedStyle::default();
  ltr_style.font_size = 16.0;
  ltr_style.direction = Direction::Ltr;
  let mut rtl_style = ltr_style.clone();
  rtl_style.direction = Direction::Rtl;

  let rect = Rect::from_xywh(0.0, 0.0, 240.0, 32.0);
  let ltr_fragment = fragment_for_control(ltr_style, control.clone(), rect);
  let rtl_fragment = fragment_for_control(rtl_style, control, rect);

  let ltr_list = DisplayListBuilder::new().build(&ltr_fragment);
  let rtl_list = DisplayListBuilder::new().build(&rtl_fragment);

  let button_label_x_ltr = text_origin_x_by_glyph_count(ltr_list.items(), |n| n > 1)
    .expect("expected file input button label text item");
  let button_label_x_rtl = text_origin_x_by_glyph_count(rtl_list.items(), |n| n > 1)
    .expect("expected file input button label text item");

  assert!(
    button_label_x_rtl > button_label_x_ltr + 60.0,
    "expected Choose File label to mirror (ltr={button_label_x_ltr}, rtl={button_label_x_rtl})"
  );

  let file_label_x_rtl = text_origin_x_by_glyph_count(rtl_list.items(), |n| n == 1)
    .expect("expected file input filename label text item");

  let button_bg = Rgba::rgb(245, 245, 245);
  let rtl_button_rect = rtl_list
    .items()
    .iter()
    .find_map(|item| match item {
      DisplayItem::FillRoundedRect(fill) if fill.color == button_bg => Some(fill.rect),
      _ => None,
    })
    .expect("expected UA painted file selector button background rect");
  let gap_from_button = rtl_button_rect.x() - file_label_x_rtl;
  assert!(
    gap_from_button > 0.0 && gap_from_button < 50.0,
    "expected RTL filename label to align to start (near the button); got gap {gap_from_button} (button_x={}, label_x={file_label_x_rtl})",
    rtl_button_rect.x()
  );
}
