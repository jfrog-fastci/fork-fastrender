use crate::geometry::Rect;
use crate::paint::display_list::DisplayItem;
use crate::paint::display_list_builder::DisplayListBuilder;
use crate::style::types::{Appearance, CaretColor};
use crate::text::caret::CaretAffinity;
use crate::tree::box_tree::{
  FormControl, FormControlKind, ImePreeditPaintState, ReplacedType, TextControlKind,
};
use crate::tree::fragment_tree::{FragmentContent, FragmentNode};
use crate::{ComputedStyle, Rgba};
use std::sync::Arc;

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

fn focused_text_input_fragment(
  style: ComputedStyle,
  value: &str,
  caret: usize,
  ime_preedit: Option<&str>,
) -> FragmentNode {
  let control = FormControl {
    control: FormControlKind::Text {
      value: value.to_string(),
      placeholder: None,
      placeholder_style: None,
      size_attr: None,
      kind: TextControlKind::Plain,
      caret,
      caret_affinity: CaretAffinity::Downstream,
      selection: None,
    },
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
    ime_preedit: ime_preedit.map(|s| ImePreeditPaintState {
      text: s.to_string(),
      cursor: None,
    }),
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

#[test]
fn ime_preedit_inserts_at_caret_in_native_input() {
  let caret_color = Rgba::new(12, 34, 56, 1.0);

  let mut style = ComputedStyle::default();
  style.font_size = 16.0;
  style.color = Rgba::BLACK;
  style.caret_color = CaretColor::Color(caret_color);

  let input_caret_1 = focused_text_input_fragment(style.clone(), "abc", 1, None);
  let input_caret_end = focused_text_input_fragment(style.clone(), "abc", 3, None);
  let input_preedit = focused_text_input_fragment(style, "abc", 1, Some("X"));

  let list_caret_1 = DisplayListBuilder::new().build(&input_caret_1);
  let list_caret_end = DisplayListBuilder::new().build(&input_caret_end);
  let list_preedit = DisplayListBuilder::new().build(&input_preedit);

  let caret_x_1 = caret_x(list_caret_1.items(), caret_color).expect("caret at idx=1");
  let caret_x_end = caret_x(list_caret_end.items(), caret_color).expect("caret at idx=end");
  let caret_x_preedit = caret_x(list_preedit.items(), caret_color).expect("caret with preedit");

  assert!(
    caret_x_1 < caret_x_preedit && caret_x_preedit < caret_x_end,
    "expected caret with preedit to be between caret@1 and caret@end (caret_x_1={caret_x_1}, caret_x_preedit={caret_x_preedit}, caret_x_end={caret_x_end})"
  );
}
