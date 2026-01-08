use fastrender::geometry::Rect;
use fastrender::paint::display_list::DisplayItem;
use fastrender::paint::display_list_builder::DisplayListBuilder;
use fastrender::style::types::Appearance;
use fastrender::tree::box_tree::{FormControl, FormControlKind, ReplacedType, TextControlKind};
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use fastrender::ComputedStyle;
use std::sync::Arc;

fn text_control(kind: TextControlKind, appearance: Appearance, value: &str) -> FormControl {
  FormControl {
    control: FormControlKind::Text {
      value: value.to_string(),
      placeholder: None,
      placeholder_style: None,
      size_attr: None,
      kind,
    },
    appearance,
    disabled: false,
    focused: false,
    focus_visible: false,
    required: false,
    invalid: false,
    placeholder_style: None,
    slider_thumb_style: None,
    slider_track_style: None,
    file_selector_button_style: None,
  }
}

fn paint_text_item_count(control: FormControl) -> usize {
  let style = Arc::new(ComputedStyle::default());
  let border_rect = Rect::from_xywh(0.0, 0.0, 120.0, 30.0);
  let fragment = FragmentNode::new_with_style(
    border_rect,
    FragmentContent::Replaced {
      replaced_type: ReplacedType::FormControl(control),
      box_id: None,
    },
    vec![],
    style,
  );
  let list = DisplayListBuilder::new().build(&fragment);
  list
    .items()
    .iter()
    .filter(|item| matches!(item, DisplayItem::Text(_)))
    .count()
}

#[test]
fn appearance_none_suppresses_number_spinner_glyphs() {
  let auto = paint_text_item_count(text_control(
    TextControlKind::Number,
    Appearance::Auto,
    "12",
  ));
  let none = paint_text_item_count(text_control(
    TextControlKind::Number,
    Appearance::None,
    "12",
  ));

  assert!(
    auto >= none + 2,
    "expected number input to paint at least two extra spinner glyph runs when appearance != none (auto={auto}, none={none})"
  );
}

#[test]
fn appearance_none_suppresses_date_dropdown_glyph() {
  let auto = paint_text_item_count(text_control(TextControlKind::Date, Appearance::Auto, ""));
  let none = paint_text_item_count(text_control(TextControlKind::Date, Appearance::None, ""));

  assert!(
    auto >= none + 1,
    "expected date-like input to paint an extra dropdown glyph run when appearance != none (auto={auto}, none={none})"
  );
}
