use fastrender::geometry::Rect;
use fastrender::paint::display_list::DisplayItem;
use fastrender::paint::display_list_builder::DisplayListBuilder;
use fastrender::style::types::Appearance;
use fastrender::tree::box_tree::{FormControl, FormControlKind, ReplacedType, TextControlKind};
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use fastrender::{ComputedStyle, Rgba};
use std::sync::Arc;

#[test]
fn placeholder_style_from_form_control_kind_is_used() {
  // When form controls are constructed directly, the placeholder pseudo style may live only on the
  // FormControlKind. The paint path should consult that field (not the legacy FormControl field).
  let mut base_style = ComputedStyle::default();
  base_style.color = Rgba::GREEN;
  base_style.font_size = 16.0;

  let mut placeholder = base_style.clone();
  placeholder.color = Rgba::RED;
  placeholder.opacity = 1.0;

  let control = FormControl {
    control: FormControlKind::Text {
      value: String::new(),
      placeholder: Some("X".to_string()),
      placeholder_style: Some(Arc::new(placeholder)),
      size_attr: None,
      kind: TextControlKind::Plain,
    },
    appearance: Appearance::Auto,
    disabled: false,
    focused: false,
    focus_visible: false,
    required: false,
    invalid: false,
    // Intentionally leave the legacy field unset; only the kind variant carries the pseudo style.
    placeholder_style: None,
    slider_thumb_style: None,
    slider_track_style: None,
  };

  let fragment = FragmentNode::new_with_style(
    Rect::from_xywh(0.0, 0.0, 120.0, 30.0),
    FragmentContent::Replaced {
      replaced_type: ReplacedType::FormControl(control),
      box_id: None,
    },
    vec![],
    Arc::new(base_style),
  );

  let list = DisplayListBuilder::new().build(&fragment);
  let text_items: Vec<_> = list
    .items()
    .iter()
    .filter_map(|item| match item {
      DisplayItem::Text(text) => Some(text),
      _ => None,
    })
    .collect();

  assert!(
    !text_items.is_empty(),
    "expected form control placeholder to emit at least one text item"
  );
  assert!(
    text_items.iter().any(|text| text.color.r > 200 && text.color.g < 50),
    "expected placeholder to use ::placeholder color from FormControlKind (got colors: {:?})",
    text_items.iter().map(|text| text.color).collect::<Vec<_>>()
  );
}

