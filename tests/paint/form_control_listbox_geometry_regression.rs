use fastrender::geometry::Rect;
use fastrender::paint::display_list::{ClipItem, ClipShape, DisplayItem};
use fastrender::paint::display_list_builder::DisplayListBuilder;
use fastrender::style::types::{Appearance, BorderStyle};
use fastrender::style::values::Length;
use fastrender::tree::box_tree::{
  FormControl, FormControlKind, ReplacedType, SelectControl, SelectItem,
};
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use fastrender::ComputedStyle;
use fastrender::Rgba;
use std::sync::Arc;

#[test]
fn listbox_select_uses_border_box_geometry_once() {
  let mut style = ComputedStyle::default();
  style.padding_top = Length::px(5.0);
  style.padding_right = Length::px(5.0);
  style.padding_bottom = Length::px(5.0);
  style.padding_left = Length::px(5.0);

  style.border_top_width = Length::px(2.0);
  style.border_right_width = Length::px(2.0);
  style.border_bottom_width = Length::px(2.0);
  style.border_left_width = Length::px(2.0);
  style.border_top_style = BorderStyle::Solid;
  style.border_right_style = BorderStyle::Solid;
  style.border_bottom_style = BorderStyle::Solid;
  style.border_left_style = BorderStyle::Solid;
  style.border_top_color = Rgba::BLACK;
  style.border_right_color = Rgba::BLACK;
  style.border_bottom_color = Rgba::BLACK;
  style.border_left_color = Rgba::BLACK;

  let style = Arc::new(style);

  let select = SelectControl {
    multiple: true,
    size: 3,
    items: Arc::new(vec![
      SelectItem::Option {
        node_id: 1,
        label: "One".to_string(),
        value: "one".to_string(),
        selected: true,
        disabled: false,
        in_optgroup: false,
      },
      SelectItem::Option {
        node_id: 2,
        label: "Two".to_string(),
        value: "two".to_string(),
        selected: false,
        disabled: false,
        in_optgroup: false,
      },
      SelectItem::Option {
        node_id: 3,
        label: "Three".to_string(),
        value: "three".to_string(),
        selected: false,
        disabled: false,
        in_optgroup: false,
      },
    ]),
    selected: vec![0],
  };

  let form_control = FormControl {
    control: FormControlKind::Select(select),
    appearance: Appearance::Auto,
    disabled: false,
    focused: false,
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
  };

  let border_rect = Rect::from_xywh(0.0, 0.0, 50.0, 40.0);
  let fragment = FragmentNode::new_with_style(
    border_rect,
    FragmentContent::Replaced {
      replaced_type: ReplacedType::FormControl(form_control),
      box_id: None,
    },
    vec![],
    style,
  );

  let list = DisplayListBuilder::new().build(&fragment);
  let items = list.items();

  let expected_content_rect = Rect::from_xywh(7.0, 7.0, 36.0, 26.0);

  let clip_start = items
    .iter()
    .position(|item| matches!(item, DisplayItem::PushClip(_)))
    .expect("listbox select should clip its rows to the content box");

  match &items[clip_start] {
    DisplayItem::PushClip(ClipItem {
      shape: ClipShape::Rect { rect, radii },
    }) => {
      assert_eq!(
        *rect, expected_content_rect,
        "expected listbox row clip to match the content box (border+padding applied once)"
      );
      assert!(
        radii.is_none(),
        "listbox row clip should not introduce extra corner radii; the outer overflow clip handles that"
      );
    }
    other => panic!("expected PushClip, got {:?}", other),
  }

  assert!(
    items.iter().any(|item| matches!(item, DisplayItem::Text(_))),
    "listbox select should paint option text"
  );
}
