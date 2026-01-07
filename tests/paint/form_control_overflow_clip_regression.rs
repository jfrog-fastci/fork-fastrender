use fastrender::geometry::Rect;
use fastrender::paint::display_list::{BorderRadii, ClipItem, ClipShape, DisplayItem};
use fastrender::paint::display_list_builder::DisplayListBuilder;
use fastrender::style::types::{Appearance, BorderCornerRadius, BorderStyle, Overflow};
use fastrender::style::values::Length;
use fastrender::tree::box_tree::{
  FormControl, FormControlKind, ReplacedType, SelectControl, SelectItem,
};
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use fastrender::{ComputedStyle, Rgba};
use std::sync::Arc;

#[test]
fn form_control_overflow_clip_uses_padding_box_not_content_box() {
  let mut style = ComputedStyle::default();
  style.overflow_x = Overflow::Clip;
  style.overflow_y = Overflow::Clip;
  style.padding_top = Length::px(5.0);
  style.padding_right = Length::px(5.0);
  style.padding_bottom = Length::px(5.0);
  style.padding_left = Length::px(5.0);
  let radius = BorderCornerRadius::uniform(Length::px(5.0));
  style.border_top_left_radius = radius;
  style.border_top_right_radius = radius;
  style.border_bottom_right_radius = radius;
  style.border_bottom_left_radius = radius;
  let style = Arc::new(style);

  let select = SelectControl {
    multiple: false,
    size: 1,
    items: Arc::new(vec![SelectItem::Option {
      label: "Label".to_string(),
      value: "value".to_string(),
      selected: true,
      disabled: false,
      in_optgroup: false,
    }]),
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
  };

  let border_rect = Rect::from_xywh(0.0, 0.0, 50.0, 20.0);
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

  let clip_start = items
    .iter()
    .position(|item| matches!(item, DisplayItem::PushClip(_)))
    .expect("form control with overflow: clip should push a clip");

  match &items[clip_start] {
    DisplayItem::PushClip(ClipItem {
      shape: ClipShape::Rect { rect, radii },
    }) => {
      // Overflow clipping for form controls should use the padding box. With border widths set to
      // zero, this should match the border box (and in particular should not be inset by padding).
      assert_eq!(*rect, border_rect);
      assert_eq!(*radii, Some(BorderRadii::uniform(5.0)));
    }
    other => panic!("expected PushClip, got {:?}", other),
  }

  assert!(
    items.iter().any(|item| matches!(item, DisplayItem::Text(_))),
    "select control should emit text items for its label/arrow"
  );
}

#[test]
fn form_control_overflow_clip_uses_padding_box_radii() {
  let mut style = ComputedStyle::default();
  style.overflow_x = Overflow::Clip;
  style.overflow_y = Overflow::Clip;
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

  let radius = BorderCornerRadius::uniform(Length::px(5.0));
  style.border_top_left_radius = radius;
  style.border_top_right_radius = radius;
  style.border_bottom_right_radius = radius;
  style.border_bottom_left_radius = radius;
  let style = Arc::new(style);

  let select = SelectControl {
    multiple: false,
    size: 1,
    items: Arc::new(vec![SelectItem::Option {
      label: "Label".to_string(),
      value: "value".to_string(),
      selected: true,
      disabled: false,
      in_optgroup: false,
    }]),
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
  };

  let border_rect = Rect::from_xywh(0.0, 0.0, 50.0, 20.0);
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

  let clip_start = items
    .iter()
    .position(|item| matches!(item, DisplayItem::PushClip(_)))
    .expect("form control with overflow: clip should push a clip");

  match &items[clip_start] {
    DisplayItem::PushClip(ClipItem {
      shape: ClipShape::Rect { rect, radii },
    }) => {
      // Padding box is border rect inset by border widths, not by padding.
      assert_eq!(*rect, Rect::from_xywh(2.0, 2.0, 46.0, 16.0));
      // Padding-box radii are border-box radii minus border widths.
      assert_eq!(*radii, Some(BorderRadii::uniform(3.0)));
    }
    other => panic!("expected PushClip, got {:?}", other),
  }
}
