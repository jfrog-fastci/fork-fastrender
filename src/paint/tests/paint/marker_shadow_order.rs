use crate::css::types::TextShadow;
use crate::geometry::Rect;
use crate::paint::display_list::DisplayItem;
use crate::paint::display_list::ListMarkerItem;
use crate::paint::display_list_builder::DisplayListBuilder;
use crate::style::color::Rgba;
use crate::style::ComputedStyle;
use crate::tree::fragment_tree::FragmentContent;
use crate::tree::fragment_tree::FragmentNode;
use crate::tree::fragment_tree::FragmentTree;
use std::sync::Arc;

#[test]
fn marker_shadow_paints_after_background() {
  let mut style = ComputedStyle::default();
  style.color = Rgba::BLACK;
  style.background_color = Rgba::WHITE;
  style.text_shadow = vec![TextShadow {
    offset_x: crate::style::values::Length::px(2.0),
    offset_y: crate::style::values::Length::px(0.0),
    blur_radius: crate::style::values::Length::px(0.0),
    color: Some(Rgba::from_rgba8(255, 0, 0, 255)),
  }]
  .into();
  let style = Arc::new(style);

  let marker = FragmentNode::new_with_style(
    Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
    FragmentContent::Text {
      text: "•".to_string().into(),
      box_id: None,
      source_range: None,
      baseline_offset: 10.0,
      shaped: None,
      is_marker: true,
      emphasis_offset: Default::default(),
      document_selection: None,
    },
    vec![],
    style,
  );
  let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 20.0, 20.0), vec![marker]);
  let tree = FragmentTree::new(root);

  let list = DisplayListBuilder::new().build_tree(&tree);

  let mut marker: Option<&ListMarkerItem> = None;
  for item in list.items() {
    if let DisplayItem::ListMarker(m) = item {
      marker = Some(m);
    }
  }

  let marker = marker.expect("marker display item");
  assert!(
    !marker.shadows.is_empty(),
    "marker shadow should be emitted"
  );
}
