use fastrender::geometry::Rect;
use fastrender::paint::display_list::{ClipItem, ClipShape, DisplayItem};
use fastrender::paint::display_list_builder::DisplayListBuilder;
use fastrender::style::types::{Appearance, Overflow};
use fastrender::style::values::Length;
use fastrender::tree::box_tree::{FormControl, FormControlKind, ReplacedType};
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use fastrender::ComputedStyle;
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
  let style = Arc::new(style);

  let form_control = FormControl {
    control: FormControlKind::Select {
      label: "Label".to_string(),
      multiple: false,
    },
    appearance: Appearance::Auto,
    disabled: false,
    focused: false,
    focus_visible: false,
    required: false,
    invalid: false,
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
      shape: ClipShape::Rect { rect, .. },
    }) => {
      // Overflow clipping for form controls should use the padding box. With border widths set to
      // zero, this should match the border box (and in particular should not be inset by padding).
      assert_eq!(*rect, border_rect);
    }
    other => panic!("expected PushClip, got {:?}", other),
  }

  assert!(
    items.iter().any(|item| matches!(item, DisplayItem::Text(_))),
    "select control should emit text items for its label/arrow"
  );
}

